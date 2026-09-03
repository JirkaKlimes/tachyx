//! The client's decode and play path.
//!
//! An Opus packet goes in, through [`AudioPlayer::push`], and the machine's own
//! speakers make a sound. Between the two sit a jitter buffer that decides when
//! each packet is due, a decoder that invents the sound of the packets that
//! never arrived, and one thread that walks the two at the rate the sound device
//! consumes.
//!
//! # The shape
//!
//! Only the last step is different on each platform, so only the last step is
//! written twice. The buffer, the Opus decoder and the volume are arithmetic
//! over bytes the host sent: they are compiled and tested wherever the workspace
//! is, rather than on the machines that have a speaker.
//!
//! # What paces it
//!
//! Nothing here owns a clock. The device does: a write to it blocks until it has
//! room, which is once every frame of sound, so the thread that decodes is timed
//! by the same crystal the speaker is. That is what makes the buffer's depth
//! mean something. A player that timed itself against its own clock would drift
//! against the device by a few parts per million, which is a click every few
//! minutes and nothing in the code to explain it.
//!
//! # Linux
//!
//! GStreamer, from an application source into `pulsesink`, which is what a
//! PipeWire machine answers with as well. The sound device's own buffer is the
//! only latency added beyond the jitter buffer.
//!
//! # Android
//!
//! AAudio in its low latency mode, written from the same thread. Every raw call
//! it takes lives in `tachyx-unsafe`, so this crate keeps the workspace ban on
//! unsafe.
//!
//! # Anywhere else
//!
//! The same names exist and [`AudioPlayer::new`] fails, so a caller gets an
//! error that says what is missing rather than a link that fails.

mod decode;
mod jitter;
mod player;
mod stats;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
mod unsupported;

#[cfg(target_os = "android")]
use android as device;
#[cfg(target_os = "linux")]
use linux as device;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
use unsupported as device;

pub use player::{AudioPlayer, Error};
pub use stats::Stats;

/// The rate everything on this path runs at, in samples a second.
///
/// Opus codes at 48 kHz whatever it was given, so a decoder asked for anything
/// else resamples on the way out. Asking for the codec's own rate is therefore
/// the one choice that costs nothing, and it is what the host captures at.
const RATE: u32 = 48_000;

/// Interleaved stereo, which is what the host encodes and what every sound
/// device here takes.
const CHANNELS: usize = 2;

/// The longest frame Opus codes, in samples of one channel.
///
/// 120 milliseconds. The host encodes ten millisecond frames, but the packet
/// says its own duration and a receiver that trusted the host's setting would
/// overrun its buffer the day that setting changed.
const MAX_FRAME: usize = 5_760;
