//! Inline video playback for nixcord, without FFmpeg.
//!
//! - Container: MP4 / QuickTime MOV via our own sample-table demuxer ([`mp4`]), so playback
//!   can start while the file is still downloading.
//! - Video: H.264 via OpenH264 (Cisco, BSD-2-Clause, compiled from source), converted from
//!   YUV to RGBA at display size on the player thread ([`yuv`]).
//! - Audio: AAC-LC and MP3 via symphonia, played with cpal; the audio device is the master
//!   clock ([`audio`], [`clock`]).
//! - HEVC, VP8/VP9, AV1, WebM/MKV: not decoded; [`player::State::Failed`] with
//!   [`player::Problem::Unsupported`] lets the UI offer the browser instead.
//!
//! The crate knows nothing about egui: [`Player`] hands out RGBA frames and a clock, and
//! calls a `wake` callback when the UI should repaint.

pub mod audio;
pub mod clock;
#[cfg(test)]
mod fixture;
pub mod h264;
pub mod mp4;
pub mod player;
pub mod probe;
pub mod source;
pub mod yuv;

pub use clock::{Frame, Pick};
pub use player::{Input, Options, Player, Problem, State, Status};
pub use probe::{Info, Prober, Target};
