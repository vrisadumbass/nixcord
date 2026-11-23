//! Async Discord Voice Gateway WebSocket + UDP/AEAD-RTP transport driver.
//!
//! Connects to the voice endpoint from [`VoiceJoinInfo`], drives the
//! [`crate::session::VoiceSession`] state machine over `tokio-tungstenite`,
//! performs UDP IP discovery, and relays encrypted/decrypted Opus frames between
//! the audio engine channels and the UDP socket.

use std::future::pending;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{
    interval_at, sleep, timeout, Instant as TokioInstant, Interval, MissedTickBehavior,
};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::crypto::{FrameCipher, PassthroughCipher, VoiceCipher};
use crate::protocol::SPEAKING_MICROPHONE;
use crate::rtp::{self, RtpHeader, OPUS_SILENCE_FRAME};
use crate::session::{Backoff, CloseAction, Outgoing, RetryDelay, VoiceAction, VoiceSession};
use crate::udp;
use crate::{VoiceCommand, VoiceEvent, VoiceJoinInfo};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Default RTP timestamp increment for a 20 ms frame at 48 kHz.
const DEFAULT_FRAME_SAMPLES: u32 = 960;
/// Stop-speaking grace window after the last outbound Opus frame.
const SPEAKING_IDLE_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum VoiceGatewayError {
    #[error("voice event receiver dropped")]
    ReceiverClosed,
    #[error("voice gateway fatal close {code}: {reason}")]
    Fatal { code: u16, reason: String },
    #[error("voice session error: {0}")]
    Session(String),
}

/// Run a Discord voice transport session using the default [`PassthroughCipher`].
///
/// Channel contracts:
/// - `outbound_opus`: `(samples_increment, opus_bytes)` from the local audio engine
///   (if `samples_increment == 0`, defaults to 960 samples = 20 ms at 48 kHz).
/// - `inbound_opus`: `(ssrc, opus_bytes)` decrypted from remote speakers for the audio engine.
pub async fn run(
    join: VoiceJoinInfo,
    commands: mpsc::UnboundedReceiver<VoiceCommand>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    outbound_opus: mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    inbound_opus: mpsc::UnboundedSender<(u32, Vec<u8>)>,
) -> Result<(), VoiceGatewayError> {
    run_with_cipher(
        join,
        commands,
        events,
        outbound_opus,
        inbound_opus,
        Box::new(PassthroughCipher),
    )
    .await
}

/// Like [`run`], with a custom [`FrameCipher`] hook (e.g. for future DAVE SFrame E2EE).
pub async fn run_with_cipher(
    join: VoiceJoinInfo,
    mut commands: mpsc::UnboundedReceiver<VoiceCommand>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    mut outbound_opus: mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    inbound_opus: mpsc::UnboundedSender<(u32, Vec<u8>)>,
    mut frame_cipher: Box<dyn FrameCipher>,
) -> Result<(), VoiceGatewayError> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let emit = |ev: VoiceEvent| {
        events
            .send(ev)
            .map_err(|_| VoiceGatewayError::ReceiverClosed)
    };

    let mut session = VoiceSession::new(
        join.guild_id,
        join.channel_id,
        join.user_id,
        join.session_id.clone(),
        join.endpoint.clone(),
    );
    let mut backoff = Backoff::default();
    let mut audio_state = AudioState::default();

    loop {
        emit(VoiceEvent::Connecting {
            guild_id: Some(join.guild_id),
            channel_id: join.channel_id,
        })?;

        let url = session.connect_url();
        tracing::info!(
            endpoint = %join.endpoint,
            resume = session.can_resume(),
            "connecting to voice gateway"
        );

        let connected = tokio::select! {
            r = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())) => r,
            cmd = commands.recv() => {
                match cmd {
                    Some(VoiceCommand::Disconnect) | None => {
                        let _ = emit(VoiceEvent::Disconnected { reason: None });
                        return Ok(());
                    }
                    Some(other) => {
                        audio_state.apply_command(&other);
                        continue;
                    }
                }
            }
        };

        let end = match connected {
            Ok(Ok((ws, _resp))) => {
                session.on_connected();
                let mut io = DriveIo {
                    emit: &emit,
                    commands: &mut commands,
                    outbound_opus: &mut outbound_opus,
                    inbound_opus: &inbound_opus,
                    frame_cipher: frame_cipher.as_mut(),
                    audio_state: &mut audio_state,
                };
                drive(ws, &join.token, &mut session, &mut backoff, &mut io).await?
            }
            Ok(Err(e)) => ConnEnd::Retry {
                reason: format!("voice connect failed: {e}"),
                delay: RetryDelay::Backoff,
            },
            Err(_) => ConnEnd::Retry {
                reason: "voice connect timed out".into(),
                delay: RetryDelay::Backoff,
            },
        };

        match end {
            ConnEnd::Disconnect => {
                let _ = emit(VoiceEvent::Disconnected { reason: None });
                return Ok(());
            }
            ConnEnd::Fatal { code, reason } => {
                tracing::error!(code, %reason, "fatal voice gateway close");
                let _ = emit(VoiceEvent::Disconnected {
                    reason: Some(reason.clone()),
                });
                return Err(if code != 0 {
                    VoiceGatewayError::Fatal { code, reason }
                } else {
                    VoiceGatewayError::Session(reason)
                });
            }
            ConnEnd::Retry { reason, delay } => {
                let wait = backoff.delay(delay, jitter());
                tracing::info!(%reason, ?wait, "voice gateway disconnected, retrying");
                tokio::select! {
                    _ = sleep(wait) => {}
                    cmd = commands.recv() => {
                        match cmd {
                            Some(VoiceCommand::Disconnect) | None => {
                                let _ = emit(VoiceEvent::Disconnected { reason: None });
                                return Ok(());
                            }
                            Some(other) => {
                                audio_state.apply_command(&other);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct AudioState {
    muted: bool,
    deafened: bool,
    rtp_seq: u16,
    rtp_ts: u32,
}

impl AudioState {
    fn apply_command(&mut self, cmd: &VoiceCommand) {
        match *cmd {
            VoiceCommand::SetMute(m) => self.muted = m,
            VoiceCommand::SetDeaf(d) => self.deafened = d,
            VoiceCommand::SetInputGain(_)
            | VoiceCommand::SetOutputVolume(_)
            | VoiceCommand::SetInputDevice(_)
            | VoiceCommand::SetOutputDevice(_)
            | VoiceCommand::Disconnect => {}
        }
    }
}

enum ConnEnd {
    Disconnect,
    Fatal { code: u16, reason: String },
    Retry { reason: String, delay: RetryDelay },
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct DriveIo<'a, E> {
    emit: &'a E,
    commands: &'a mut mpsc::UnboundedReceiver<VoiceCommand>,
    outbound_opus: &'a mut mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    inbound_opus: &'a mpsc::UnboundedSender<(u32, Vec<u8>)>,
    frame_cipher: &'a mut dyn FrameCipher,
    audio_state: &'a mut AudioState,
}

struct UdpTransport {
    socket: UdpSocket,
    server_addr: SocketAddr,
    cipher: Option<VoiceCipher>,
    ssrc: u32,
}

async fn drive<E>(
    ws: Ws,
    token: &str,
    session: &mut VoiceSession,
    backoff: &mut Backoff,
    io: &mut DriveIo<'_, E>,
) -> Result<ConnEnd, VoiceGatewayError>
where
    E: Fn(VoiceEvent) -> Result<(), VoiceGatewayError>,
{
    let (mut sink, mut stream) = ws.split();
    let mut heartbeat: Option<Interval> = None;
    let hello_deadline = sleep(HELLO_TIMEOUT);
    tokio::pin!(hello_deadline);

    let mut udp_transport: Option<UdpTransport> = None;
    let mut udp_buf = vec![0u8; 4096];
    let mut self_speaking = false;
    let mut speaking_idle_deadline: Option<TokioInstant> = None;

    async fn close<S>(sink: &mut S, code: CloseCode)
    where
        S: futures_util::Sink<Message> + Unpin,
    {
        let frame = CloseFrame {
            code,
            reason: "".into(),
        };
        let _ = timeout(CLOSE_TIMEOUT, sink.send(Message::Close(Some(frame)))).await;
    }

    loop {
        let actions = tokio::select! {
            _ = &mut hello_deadline, if heartbeat.is_none() => {
                close(&mut sink, CloseCode::Library(4000)).await;
                return Ok(ConnEnd::Retry {
                    reason: "no Hello from voice gateway".into(),
                    delay: RetryDelay::Backoff,
                });
            }
            _ = tick(&mut heartbeat) => session.on_heartbeat_tick(Instant::now()),
            cmd = io.commands.recv() => {
                match cmd {
                    Some(VoiceCommand::Disconnect) | None => {
                        close(&mut sink, CloseCode::Normal).await;
                        return Ok(ConnEnd::Disconnect);
                    }
                    Some(other) => {
                        io.audio_state.apply_command(&other);
                        if io.audio_state.muted && self_speaking {
                            self_speaking = false;
                            speaking_idle_deadline = None;
                            if let Some(t) = udp_transport.as_mut() {
                                send_silence_burst(t, io.audio_state, io.frame_cipher).await;
                                let payload = session.payload(
                                    Outgoing::Speaking {
                                        speaking: 0,
                                        delay: 0,
                                        ssrc: t.ssrc,
                                    },
                                    token,
                                );
                                let _ = sink.send(Message::text(payload)).await;
                            }
                            (io.emit)(VoiceEvent::SelfSpeaking(false))?;
                        }
                        Vec::new()
                    }
                }
            }
            out_frame = io.outbound_opus.recv(), if udp_transport.as_ref().is_some_and(|t| t.cipher.is_some()) => {
                if let Some((samples, opus)) = out_frame {
                    if !io.audio_state.muted {
                        if let Some(t) = udp_transport.as_mut() {
                            if !self_speaking {
                                self_speaking = true;
                                let payload = session.payload(
                                    Outgoing::Speaking {
                                        speaking: SPEAKING_MICROPHONE,
                                        delay: 0,
                                        ssrc: t.ssrc,
                                    },
                                    token,
                                );
                                if let Err(e) = sink.send(Message::text(payload)).await {
                                    return Ok(ConnEnd::Retry {
                                        reason: format!("voice send failed: {e}"),
                                        delay: RetryDelay::Backoff,
                                    });
                                }
                                (io.emit)(VoiceEvent::SelfSpeaking(true))?;
                            }
                            speaking_idle_deadline = Some(TokioInstant::now() + SPEAKING_IDLE_TIMEOUT);
                            send_opus_packet(t, io.audio_state, io.frame_cipher, samples, &opus).await;
                        }
                    }
                }
                Vec::new()
            }
            _ = sleep_until_opt(speaking_idle_deadline), if self_speaking => {
                self_speaking = false;
                speaking_idle_deadline = None;
                if let Some(t) = udp_transport.as_mut() {
                    send_silence_burst(t, io.audio_state, io.frame_cipher).await;
                    let payload = session.payload(
                        Outgoing::Speaking {
                            speaking: 0,
                            delay: 0,
                            ssrc: t.ssrc,
                        },
                        token,
                    );
                    if let Err(e) = sink.send(Message::text(payload)).await {
                        return Ok(ConnEnd::Retry {
                            reason: format!("voice send failed: {e}"),
                            delay: RetryDelay::Backoff,
                        });
                    }
                }
                (io.emit)(VoiceEvent::SelfSpeaking(false))?;
                Vec::new()
            }
            udp_res = recv_udp(&udp_transport, &mut udp_buf) => {
                if let Ok((len, from)) = udp_res {
                    if let Some(t) = udp_transport.as_ref() {
                        if from == t.server_addr && !io.audio_state.deafened {
                            let pkt = &udp_buf[..len];
                            if !rtp::is_rtcp(pkt) {
                                if let Some(cipher) = t.cipher.as_ref() {
                                    match cipher.decrypt_rtp(pkt) {
                                        Ok((hdr, opus)) => {
                                            match io.frame_cipher.decrypt_frame(hdr.ssrc, &opus) {
                                                Ok(plain) => {
                                                    let _ = io.inbound_opus.send((hdr.ssrc, plain));
                                                }
                                                Err(e) => {
                                                    tracing::debug!(error = %e, ssrc = hdr.ssrc, "frame cipher decrypt failed");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::debug!(error = %e, "UDP RTP decrypt failed");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Vec::new()
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => session.handle_text(text.as_str(), Instant::now()),
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.as_ref().map(|f| u16::from(f.code));
                    return Ok(match session.on_close(code) {
                        CloseAction::Fatal { code, reason } => ConnEnd::Fatal {
                            code,
                            reason: format!("{reason} ({code})"),
                        },
                        CloseAction::Retry { delay } => ConnEnd::Retry {
                            reason: match code {
                                Some(c) => format!("{} ({c})", crate::session::close_reason(c)),
                                None => "voice connection closed".into(),
                            },
                            delay,
                        },
                    });
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    return Ok(ConnEnd::Retry {
                        reason: format!("voice websocket error: {e}"),
                        delay: RetryDelay::Backoff,
                    });
                }
                None => {
                    return Ok(ConnEnd::Retry {
                        reason: "voice connection closed".into(),
                        delay: RetryDelay::Backoff,
                    });
                }
            },
        };

        let mut pending_actions = actions;
        while !pending_actions.is_empty() {
            let batch = std::mem::take(&mut pending_actions);
            for action in batch {
                match action {
                    VoiceAction::Send(out) => {
                        let payload = session.payload(out, token);
                        if let Err(e) = sink.send(Message::text(payload)).await {
                            return Ok(ConnEnd::Retry {
                                reason: format!("voice send failed: {e}"),
                                delay: RetryDelay::Backoff,
                            });
                        }
                    }
                    VoiceAction::StartHeartbeat(every) => {
                        let first = crate::session::first_heartbeat_delay(every, jitter());
                        let mut iv = interval_at(TokioInstant::now() + first, every);
                        iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        heartbeat = Some(iv);
                    }
                    VoiceAction::DiscoverIp { ip, port, ssrc, .. } => {
                        let server_ip: IpAddr = match ip.parse() {
                            Ok(addr) => addr,
                            Err(e) => {
                                return Ok(ConnEnd::Fatal {
                                    code: 0,
                                    reason: format!("invalid voice server IP '{ip}': {e}"),
                                });
                            }
                        };
                        let server_addr = SocketAddr::new(server_ip, port);
                        let bind_addr = if server_addr.is_ipv6() {
                            "[::]:0"
                        } else {
                            "0.0.0.0:0"
                        };
                        let socket = match UdpSocket::bind(bind_addr).await {
                            Ok(s) => s,
                            Err(e) => {
                                return Ok(ConnEnd::Retry {
                                    reason: format!("UDP bind failed: {e}"),
                                    delay: RetryDelay::Backoff,
                                });
                            }
                        };
                        match udp::discover_ip(&socket, server_addr, ssrc).await {
                            Ok(external_addr) => {
                                tracing::info!(%external_addr, %server_addr, "voice UDP IP discovery succeeded");
                                udp_transport = Some(UdpTransport {
                                    socket,
                                    server_addr,
                                    cipher: None,
                                    ssrc,
                                });
                                pending_actions.extend(session.on_ip_discovered(external_addr));
                            }
                            Err(e) => {
                                close(&mut sink, CloseCode::Library(4000)).await;
                                return Ok(ConnEnd::Retry {
                                    reason: format!("UDP IP discovery failed: {e}"),
                                    delay: RetryDelay::Backoff,
                                });
                            }
                        }
                    }
                    VoiceAction::ConfigureCrypto {
                        secret_key, ssrc, ..
                    } => {
                        let cipher = match VoiceCipher::new(&secret_key) {
                            Ok(c) => c,
                            Err(e) => {
                                return Ok(ConnEnd::Fatal {
                                    code: 0,
                                    reason: format!("failed to initialize voice cipher: {e}"),
                                });
                            }
                        };
                        if let Some(t) = udp_transport.as_mut() {
                            t.cipher = Some(cipher);
                            t.ssrc = ssrc;
                        }
                    }
                    VoiceAction::Emit(ev) => {
                        if matches!(ev, VoiceEvent::Connected { .. }) {
                            backoff.reset();
                        }
                        (io.emit)(ev)?;
                    }
                    VoiceAction::Reconnect { reason, delay } => {
                        close(&mut sink, CloseCode::Library(4000)).await;
                        return Ok(ConnEnd::Retry { reason, delay });
                    }
                    VoiceAction::Fatal { reason } => {
                        close(&mut sink, CloseCode::Normal).await;
                        return Ok(ConnEnd::Fatal { code: 0, reason });
                    }
                }
            }
        }
    }
}

async fn send_opus_packet(
    transport: &mut UdpTransport,
    audio_state: &mut AudioState,
    frame_cipher: &mut dyn FrameCipher,
    samples: u32,
    opus: &[u8],
) {
    let Some(cipher) = transport.cipher.as_mut() else {
        return;
    };
    let payload = match frame_cipher.encrypt_frame(transport.ssrc, opus) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "frame cipher encrypt failed");
            return;
        }
    };
    let hdr = RtpHeader::opus(audio_state.rtp_seq, audio_state.rtp_ts, transport.ssrc);
    audio_state.rtp_seq = audio_state.rtp_seq.wrapping_add(1);
    let step = if samples == 0 {
        DEFAULT_FRAME_SAMPLES
    } else {
        samples
    };
    audio_state.rtp_ts = audio_state.rtp_ts.wrapping_add(step);

    match cipher.encrypt_rtp(&hdr, &payload) {
        Ok(pkt) => {
            if let Err(e) = transport.socket.send_to(&pkt, transport.server_addr).await {
                tracing::debug!(error = %e, "UDP RTP send failed");
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "RTP AEAD encrypt failed");
        }
    }
}

async fn send_silence_burst(
    transport: &mut UdpTransport,
    audio_state: &mut AudioState,
    frame_cipher: &mut dyn FrameCipher,
) {
    for _ in 0..rtp::OPUS_SILENCE_BURST_LEN {
        send_opus_packet(
            transport,
            audio_state,
            frame_cipher,
            DEFAULT_FRAME_SAMPLES,
            &OPUS_SILENCE_FRAME,
        )
        .await;
    }
}

async fn recv_udp(
    transport: &Option<UdpTransport>,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
    match transport {
        Some(t) => t.socket.recv_from(buf).await,
        None => pending().await,
    }
}

async fn tick(heartbeat: &mut Option<Interval>) {
    match heartbeat {
        Some(iv) => {
            iv.tick().await;
        }
        None => pending().await,
    }
}

async fn sleep_until_opt(at: Option<TokioInstant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => pending().await,
    }
}

fn jitter() -> f64 {
    use std::hash::BuildHasher;
    let h = std::collections::hash_map::RandomState::new().hash_one(Instant::now());
    (h >> 11) as f64 / (1u64 << 53) as f64
}
