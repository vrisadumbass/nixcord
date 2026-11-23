//! Image loading for nixcord: fetch over HTTPS, decode off the UI thread, downscale to the
//! size it will be displayed at, and hand back plain RGBA pixels.
//!
//! The crate is UI-agnostic (no egui types). The app wraps it in a texture cache.
//!
//! Memory rules:
//! - Full-resolution pixels never leave the decode thread: every image is downscaled to the
//!   requested pixel box before it is returned.
//! - Downloads are capped ([`Config::max_bytes`]) and decoder allocations are limited.
//! - At most [`Config::max_in_flight`] downloads and [`Config::max_decodes`] decodes run at
//!   once, which bounds the transient peak.
//! - The background runtime is started by the caller only when the first image is needed.

pub mod demo;
pub mod disk;
pub mod lru;
pub mod size;
pub mod url;

mod decode;
mod fetch;

pub use decode::{decode, encode_png};
pub use fetch::{Config, Fetcher, SaveDone};

use std::time::Duration;

/// Decoded, downscaled image: tightly packed, non-premultiplied RGBA8.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// Bytes held by the pixel buffer.
    pub fn byte_size(&self) -> usize {
        self.rgba.len()
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

/// What to load: a URL, decoded to fit inside `max_w` x `max_h` pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub url: String,
    pub max_w: u32,
    pub max_h: u32,
}

impl Key {
    pub fn new(url: impl Into<String>, max_w: u32, max_h: u32) -> Self {
        Self {
            url: url.into(),
            max_w: max_w.max(1),
            max_h: max_h.max(1),
        }
    }

    /// Compact id for maps on the UI side (FNV-1a of the URL mixed with the box size).
    pub fn id(&self) -> u64 {
        url::sized_id(&self.url, self.max_w, self.max_h)
    }
}

/// Why an image could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// 403/404/410: the URL is dead (or its signature expired).
    NotFound,
    /// Other non-success HTTP status.
    Status(u16),
    /// Connection error or timeout.
    Network,
    /// Larger than [`Config::max_bytes`].
    TooLarge,
    /// Unsupported format, corrupt data, or decoder limits exceeded.
    Decode,
}

impl Failure {
    /// How long to wait before trying this URL again.
    pub fn retry_after(self) -> Duration {
        match self {
            Failure::Network => Duration::from_secs(30),
            Failure::Status(s) if s == 429 || s >= 500 => Duration::from_secs(60),
            // Permanent for this session (practically).
            _ => Duration::from_secs(60 * 60),
        }
    }
}

/// A finished request, as received from [`Fetcher::try_recv`].
#[derive(Debug)]
pub struct Loaded {
    pub key: Key,
    pub result: Result<Image, Failure>,
}
