//! Token persistence: OS credential store (keyring) plus the `NIXCORD_TOKEN` env var.
//! The token is never logged or printed; errors are logged without it.

const SERVICE: &str = "nixcord";
const ACCOUNT: &str = "discord-token";

/// Where a startup token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Keyring,
}

/// Token to auto-login with, if any. The env var wins over the keyring.
pub fn startup_token() -> Option<(String, Source)> {
    if let Ok(t) = std::env::var("NIXCORD_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some((t, Source::Env));
        }
    }
    load().map(|t| (t, Source::Keyring))
}

fn entry() -> Option<keyring::Entry> {
    match keyring::Entry::new(SERVICE, ACCOUNT) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!("credential store unavailable: {e}");
            None
        }
    }
}

pub fn load() -> Option<String> {
    match entry()?.get_password() {
        Ok(t) if !t.trim().is_empty() => Some(t),
        Ok(_) | Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!("could not read saved token: {e}");
            None
        }
    }
}

pub fn save(token: &str) {
    if let Some(e) = entry() {
        if let Err(err) = e.set_password(token) {
            tracing::warn!("could not save token: {err}");
        }
    }
}

pub fn clear() {
    if let Some(e) = entry() {
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(err) => tracing::warn!("could not delete saved token: {err}"),
        }
    }
}
