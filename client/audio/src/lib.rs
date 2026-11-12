//! Remote audio (docs/38-remote-audio.md): what the remote computer plays reaches
//! the viewer's speakers.
//!
//! - [`capture`]: the sound the computer sends to its speakers (Windows: WASAPI
//!   loopback of the default output device; Linux: the monitor of the default
//!   output through PipeWire or PulseAudio; macOS: ScreenCaptureKit's system
//!   audio).
//! - [`opus`]: Opus at 48 kHz stereo, 20 ms packets.
//! - [`packet`]: the QUIC datagram that carries one packet.
//! - [`player`]: playback on the viewer's default output device, with a short
//!   buffer against jitter and Opus's own concealment of lost packets.

pub mod capture;
pub mod opus;
pub mod packet;
pub mod player;
#[cfg(target_os = "linux")]
mod pulse;
mod resample;

/// Opus works at 48 kHz: other device rates are converted.
pub const RATE: u32 = 48_000;
/// Stereo on the wire; mono or surround devices are mixed to it and back.
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet: 20 ms.
pub const FRAME: usize = 960;
