//! What the client has to read out of the bytes it is handed.
//!
//! The host repeats the parameter sets in the stream before every key frame, so
//! the decoder configures itself and needs no separate configuration step, no
//! step to repeat after a reconnection, and none after a resolution change.
//! Two things still have to be read here, and both are read from the stream
//! rather than from the host's stream configuration message, because the stream
//! is what the decoder is actually going to see:
//!
//! - the coded size, which `MediaCodec` demands before it will start, and which
//!   changes mid-stream when the host changes resolution;
//! - whether a unit can be decoded on its own, which is where a decoder that has
//!   fallen behind is allowed to start again.
//!
//! One reader per codec the client decodes, behind [`Unit::of`]. Everything here
//! is arithmetic over a byte slice, so all of it is exercised by the tests
//! against parameter sets taken off real encoders.

mod av1;
mod h264;

use tachyx_proto::control::Codec;

/// The picture size the stream's parameter set declares, in pixels.
///
/// This is what the decoder writes and what the surface it writes into has to be
/// sized for. It is not always what the viewer should see: H.264 states a size
/// the macroblock grid cannot hold, such as 1080 rows, by coding the next
/// multiple of sixteen up and attaching a crop rectangle, and an AV1 encoder may
/// round the same way. Nothing on the Android present path applies a crop, so
/// the visible size travels in the host's stream configuration message and the
/// application applies it when it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Coded {
    /// Width in pixels.
    pub(super) width: u16,
    /// Height in pixels.
    pub(super) height: u16,
}

/// What one access unit turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unit {
    /// The coded size the parameter set it carries declares, if it carries one
    /// that parses.
    pub(super) coded: Option<Coded>,
    /// Whether it can be decoded with nothing before it.
    pub(super) key: bool,
}

impl Unit {
    /// Reads what one access unit of `codec` contains.
    ///
    /// Only the two facts above are wanted, and reading stops at the coded
    /// picture: the parameter sets come before it and it is the end of the
    /// unit, so nothing past there can change either answer. A unit that does
    /// not parse holds nothing, which drops it rather than feeding a decoder
    /// bytes that were never what they claimed.
    pub(super) fn of(codec: Codec, bytes: &[u8]) -> Self {
        match codec {
            Codec::H264 => h264::unit(bytes),
            Codec::Av1 => av1::unit(bytes),
        }
    }
}
