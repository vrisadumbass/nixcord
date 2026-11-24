//! Discord Audio & Video End-to-End Encryption (DAVE) session manager backed by `davey`.
//!
//! Handles the Voice Gateway v8 DAVE control plane (Op 11, 13, 21–31) and
//! per-frame SFrame encryption/decryption of Opus packets.

use std::collections::HashSet;
use std::num::NonZeroU16;

use davey::{DaveSession, MediaType, ProposalsOperationType, DAVE_PROTOCOL_VERSION};
use nixcord_model::Snowflake;

use crate::crypto::CryptoError;
use crate::protocol;

/// Outgoing Voice Gateway messages emitted by [`DaveManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaveOutgoing {
    /// JSON text frame (Op 23 ReadyForTransition, Op 31 InvalidCommitWelcome).
    Json(String),
    /// Binary frame without sequence prefix (Op 26 KeyPackage, Op 28 CommitWelcome).
    Binary(Vec<u8>),
}

/// Stateful DAVE E2EE session controller for one voice channel connection.
pub struct DaveManager {
    user_id: u64,
    channel_id: u64,
    protocol_version: u16,
    recognized_users: HashSet<u64>,
    session: Option<DaveSession>,
}

impl DaveManager {
    pub fn new(user_id: Snowflake, channel_id: Snowflake) -> Self {
        let mut recognized_users = HashSet::new();
        recognized_users.insert(user_id.0);
        Self {
            user_id: user_id.0,
            channel_id: channel_id.0,
            protocol_version: 0,
            recognized_users,
            session: None,
        }
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    pub fn is_ready(&self) -> bool {
        self.session.as_ref().is_some_and(DaveSession::is_ready)
    }

    pub fn on_clients_connect(&mut self, user_ids: &[Snowflake]) {
        for &uid in user_ids {
            self.recognized_users.insert(uid.0);
        }
    }

    pub fn on_client_disconnect(&mut self, user_id: Snowflake) {
        if user_id.0 != self.user_id {
            self.recognized_users.remove(&user_id.0);
        }
    }

    /// Handle `dave_protocol_version` from Op 4 Session Description.
    pub fn on_session_description(&mut self, protocol_version: u16) -> Vec<DaveOutgoing> {
        self.protocol_version = protocol_version;
        if protocol_version == 0 {
            if let Some(session) = self.session.as_mut() {
                let _ = session.reset();
                session.set_passthrough_mode(true, Some(10));
            }
            return Vec::new();
        }
        self.init_or_reinit_and_key_package(protocol_version)
    }

    /// Handle Op 21 `DAVE_PROTOCOL_PREPARE_TRANSITION`.
    pub fn on_prepare_transition(
        &mut self,
        transition_id: u16,
        protocol_version: u16,
    ) -> Vec<DaveOutgoing> {
        self.protocol_version = protocol_version;
        if let Some(session) = self.session.as_mut() {
            if protocol_version == 0 {
                session.set_passthrough_mode(true, Some(10));
            }
            if let Err(e) = session.prepare_transition(transition_id, protocol_version) {
                tracing::warn!(transition_id, protocol_version, error = %e, "DAVE prepare_transition failed");
            }
        }
        if transition_id != 0 {
            vec![DaveOutgoing::Json(
                protocol::dave_ready_for_transition_payload(transition_id),
            )]
        } else {
            Vec::new()
        }
    }

    /// Handle Op 22 `DAVE_PROTOCOL_EXECUTE_TRANSITION`.
    pub fn on_execute_transition(&mut self, transition_id: u16) {
        if let Some(session) = self.session.as_mut() {
            match session.execute_transition(transition_id) {
                Ok(executed) => {
                    tracing::debug!(transition_id, executed, "DAVE execute_transition");
                    if self.protocol_version > 0 {
                        session.set_passthrough_mode(false, Some(10));
                    }
                }
                Err(e) => {
                    tracing::warn!(transition_id, error = %e, "DAVE execute_transition failed");
                }
            }
        }
    }

    /// Handle Op 24 `DAVE_PROTOCOL_PREPARE_EPOCH`.
    pub fn on_prepare_epoch(&mut self, epoch: u64, protocol_version: u16) -> Vec<DaveOutgoing> {
        self.protocol_version = protocol_version;
        if epoch == 1 && protocol_version > 0 {
            return self.init_or_reinit_and_key_package(protocol_version);
        }
        Vec::new()
    }

    /// Handle incoming binary Voice Gateway v8 DAVE frames (Op 25, 27, 29, 30).
    pub fn on_binary(&mut self, op: u8, payload: &[u8]) -> Vec<DaveOutgoing> {
        match op {
            protocol::OP_DAVE_MLS_EXTERNAL_SENDER => {
                if self.session.is_none() {
                    let version = if self.protocol_version > 0 {
                        self.protocol_version
                    } else {
                        DAVE_PROTOCOL_VERSION
                    };
                    if let Some(v) = NonZeroU16::new(version) {
                        match DaveSession::new(v, self.user_id, self.channel_id, None) {
                            Ok(s) => {
                                self.protocol_version = version;
                                self.session = Some(s);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "DAVE init on external sender failed");
                                return Vec::new();
                            }
                        }
                    }
                }
                if let Some(session) = self.session.as_mut() {
                    if let Err(e) = session.set_external_sender(payload) {
                        tracing::warn!(error = %e, "DAVE set_external_sender failed");
                    }
                }
                Vec::new()
            }
            protocol::OP_DAVE_MLS_PROPOSALS => {
                let Some((&op_byte, proposals)) = payload.split_first() else {
                    tracing::warn!("empty DAVE MLS proposals frame");
                    return Vec::new();
                };
                let op_type = match op_byte {
                    0 => ProposalsOperationType::APPEND,
                    1 => ProposalsOperationType::REVOKE,
                    other => {
                        tracing::warn!(op_type = other, "unknown DAVE proposals operation type");
                        return Vec::new();
                    }
                };
                let recognized = self.recognized_vec();
                let Some(session) = self.session.as_mut() else {
                    tracing::warn!("received DAVE proposals without active session");
                    return Vec::new();
                };
                match session.process_proposals(op_type, proposals, Some(&recognized)) {
                    Ok(Some(cw)) => {
                        let mut bin = Vec::with_capacity(
                            1 + cw.commit.len() + cw.welcome.as_ref().map_or(0, Vec::len),
                        );
                        bin.push(protocol::OP_DAVE_MLS_COMMIT_WELCOME);
                        bin.extend_from_slice(&cw.commit);
                        if let Some(welcome) = cw.welcome {
                            bin.extend_from_slice(&welcome);
                        }
                        vec![DaveOutgoing::Binary(bin)]
                    }
                    Ok(None) => Vec::new(),
                    Err(e) => {
                        tracing::warn!(error = %e, "DAVE process_proposals failed");
                        Vec::new()
                    }
                }
            }
            protocol::OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION => {
                if payload.len() < 2 {
                    tracing::warn!("truncated DAVE announce commit frame");
                    return Vec::new();
                }
                let transition_id = u16::from_be_bytes([payload[0], payload[1]]);
                let commit = &payload[2..];
                let protocol_version = self.protocol_version;
                let Some(session) = self.session.as_mut() else {
                    tracing::warn!("received DAVE commit without active session");
                    return Vec::new();
                };
                match session.process_commit(commit) {
                    Ok(true) => {
                        if let Err(e) = session.prepare_transition(transition_id, protocol_version)
                        {
                            tracing::warn!(transition_id, error = %e, "DAVE prepare_transition after commit failed");
                        }
                        if transition_id != 0 {
                            vec![DaveOutgoing::Json(
                                protocol::dave_ready_for_transition_payload(transition_id),
                            )]
                        } else {
                            Vec::new()
                        }
                    }
                    Ok(false) => Vec::new(),
                    Err(e) => {
                        tracing::warn!(transition_id, error = %e, "DAVE process_commit failed; requesting recovery");
                        self.recover_invalid_transition(transition_id)
                    }
                }
            }
            protocol::OP_DAVE_MLS_WELCOME => {
                if payload.len() < 2 {
                    tracing::warn!("truncated DAVE welcome frame");
                    return Vec::new();
                }
                let transition_id = u16::from_be_bytes([payload[0], payload[1]]);
                let welcome = &payload[2..];
                let recognized = self.recognized_vec();
                let protocol_version = self.protocol_version;
                let Some(session) = self.session.as_mut() else {
                    tracing::warn!("received DAVE welcome without active session");
                    return Vec::new();
                };
                match session.process_welcome(welcome, Some(&recognized)) {
                    Ok(()) => {
                        if let Err(e) = session.prepare_transition(transition_id, protocol_version)
                        {
                            tracing::warn!(transition_id, error = %e, "DAVE prepare_transition after welcome failed");
                        }
                        if transition_id != 0 {
                            vec![DaveOutgoing::Json(
                                protocol::dave_ready_for_transition_payload(transition_id),
                            )]
                        } else {
                            Vec::new()
                        }
                    }
                    Err(e) => {
                        tracing::warn!(transition_id, error = %e, "DAVE process_welcome failed; requesting recovery");
                        self.recover_invalid_transition(transition_id)
                    }
                }
            }
            other => {
                tracing::debug!(op = other, "ignoring unhandled binary voice gateway opcode");
                Vec::new()
            }
        }
    }

    /// End-to-end encrypt an outbound Opus packet when DAVE is active & ready.
    pub fn encrypt_opus(&mut self, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if self.protocol_version == 0 {
            return Ok(frame.to_vec());
        }
        let Some(session) = self.session.as_mut() else {
            return Ok(frame.to_vec());
        };
        if !session.can_encrypt() {
            return Ok(frame.to_vec());
        }
        session
            .encrypt_opus(frame)
            .map(|c| c.into_owned())
            .map_err(|e| CryptoError::FrameCipher(e.to_string()))
    }

    /// End-to-end decrypt an inbound Opus packet from `user_id` when DAVE is active & ready.
    pub fn decrypt_opus(
        &mut self,
        user_id: Option<Snowflake>,
        frame: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if self.protocol_version == 0 || frame == davey::OPUS_SILENCE_PACKET {
            return Ok(frame.to_vec());
        }
        let Some(session) = self.session.as_mut() else {
            return Ok(frame.to_vec());
        };
        if !session.is_ready() {
            return Ok(frame.to_vec());
        }
        let Some(uid) = user_id else {
            return Err(CryptoError::FrameCipher(
                "unknown speaker user_id for SSRC".into(),
            ));
        };
        session
            .decrypt(uid.0, MediaType::AUDIO, frame)
            .map_err(|e| CryptoError::FrameCipher(e.to_string()))
    }

    fn recognized_vec(&self) -> Vec<u64> {
        self.recognized_users.iter().copied().collect()
    }

    fn init_or_reinit_and_key_package(&mut self, protocol_version: u16) -> Vec<DaveOutgoing> {
        let Some(version) = NonZeroU16::new(protocol_version) else {
            return Vec::new();
        };
        let init_res = match self.session.as_mut() {
            Some(session) => session
                .reinit(version, self.user_id, self.channel_id, None)
                .map_err(|e| e.to_string()),
            None => DaveSession::new(version, self.user_id, self.channel_id, None)
                .map(|s| {
                    self.session = Some(s);
                })
                .map_err(|e| e.to_string()),
        };
        if let Err(e) = init_res {
            tracing::warn!(protocol_version, error = %e, "DAVE session init/reinit failed");
            return Vec::new();
        }
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        match session.create_key_package() {
            Ok(kp) => {
                let mut bin = Vec::with_capacity(1 + kp.len());
                bin.push(protocol::OP_DAVE_MLS_KEY_PACKAGE);
                bin.extend_from_slice(&kp);
                vec![DaveOutgoing::Binary(bin)]
            }
            Err(e) => {
                tracing::warn!(error = %e, "DAVE create_key_package failed");
                Vec::new()
            }
        }
    }

    fn recover_invalid_transition(&mut self, transition_id: u16) -> Vec<DaveOutgoing> {
        let mut out = vec![DaveOutgoing::Json(
            protocol::dave_invalid_commit_welcome_payload(transition_id),
        )];
        if self.protocol_version > 0 {
            out.extend(self.init_or_reinit_and_key_package(self.protocol_version));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_description_v1_emits_op26_key_package() {
        let mut mgr = DaveManager::new(Snowflake(100), Snowflake(200));
        let out = mgr.on_session_description(DAVE_PROTOCOL_VERSION);
        assert_eq!(out.len(), 1);
        match &out[0] {
            DaveOutgoing::Binary(bytes) => {
                assert_eq!(bytes[0], protocol::OP_DAVE_MLS_KEY_PACKAGE);
                assert!(bytes.len() > 32, "MLS key package should be non-trivial");
            }
            other => panic!("expected binary Op 26 key package, got {other:?}"),
        }
        assert_eq!(mgr.protocol_version(), DAVE_PROTOCOL_VERSION);
    }

    #[test]
    fn protocol_version_zero_passes_opus_through() {
        let mut mgr = DaveManager::new(Snowflake(100), Snowflake(200));
        assert!(mgr.on_session_description(0).is_empty());
        let sample = vec![0xAA, 0xBB, 0xCC, 0xDD];
        assert_eq!(mgr.encrypt_opus(&sample).unwrap(), sample);
        assert_eq!(
            mgr.decrypt_opus(Some(Snowflake(300)), &sample).unwrap(),
            sample
        );
    }

    #[test]
    fn prepare_transition_nonzero_emits_op23_ready() {
        let mut mgr = DaveManager::new(Snowflake(100), Snowflake(200));
        let out = mgr.on_prepare_transition(7, 0);
        assert_eq!(out.len(), 1);
        match &out[0] {
            DaveOutgoing::Json(s) => {
                let v: serde_json::Value = serde_json::from_str(s).unwrap();
                assert_eq!(v["op"], protocol::OP_DAVE_READY_FOR_TRANSITION);
                assert_eq!(v["d"]["transition_id"], 7);
            }
            other => panic!("expected JSON Op 23, got {other:?}"),
        }
    }
}
