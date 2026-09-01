//! The Android decode and present path.
//!
//! The platform decoder writes into the surface of a Flutter surface producer.
//! From API 29 that producer is an image reader, and the buffers the decoder
//! fills reach the compositor's Vulkan backend as they are, so a picture goes
//! from the decoder to the screen without being copied, converted or even
//! mapped. Nothing in this module ever looks at a decoded byte: it hands the
//! decoder an access unit at one end and says which buffer to draw at the other.
//!
//! The producer is not the older surface texture, which Flutter's own source
//! describes as costing a copy on the device per frame and as existing to
//! migrate away from. That is also why `irondash_texture`, which the Linux path
//! uses, is not the answer here.
//!
//! Two of the modules below are arithmetic over bytes and reach no platform call
//! at all. They are compiled everywhere so that their tests run wherever the
//! workspace's tests are run, rather than only where a phone is attached.

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod bitstream;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod hints;

#[cfg(target_os = "android")]
mod decoder;
#[cfg(target_os = "android")]
mod present;

#[cfg(target_os = "android")]
pub use present::{Error, VideoTexture};
