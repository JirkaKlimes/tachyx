//! The client's decode and present path.
//!
//! An encoded H.264 access unit goes in; a Flutter texture identifier comes out,
//! and no frame byte is ever read by the CPU. The mechanism is different on each
//! platform because the embedder's is, and the two share only their shape:
//! [`VideoTexture::new`], [`VideoTexture::push`], and the counters in [`Stats`].
//!
//! # Linux
//!
//! GStreamer decodes on the video engine, exports the decoded surface as a
//! shared buffer, imports that buffer as an OpenGL image and converts it to the
//! one format Flutter's Linux embedder accepts. The pipeline runs in the
//! embedder's own share group, so the texture Flutter draws is the texture the
//! decoder wrote.
//!
//! Three properties are stated in the pipeline's caps rather than checked in
//! code, so a machine that cannot honour one fails to negotiate instead of
//! quietly falling back to a copy:
//!
//! - `video/x-raw(memory:DMABuf)` between the decoder and the uploader. Without
//!   it the decoder downloads every frame to system memory.
//! - `texture-target=2D`. Flutter's Impeller renderer ignores the texture target
//!   the callback returns and samples as if it were `GL_TEXTURE_2D`, so anything
//!   else draws nothing.
//! - `format=RGBA`. The embedder states `GL_RGBA8` for every texture it is given
//!   and never asks what the texture really is.
//!
//! # Android
//!
//! The platform decoder writes into the surface of a Flutter surface producer,
//! which on this API level is an image reader whose buffers reach the
//! compositor's Vulkan backend directly. A frame is never mapped, copied or even
//! named here: the client hands the decoder an access unit at one end and tells
//! it to render at the other.
//!
//! # Anywhere else
//!
//! The same names exist and every one of them fails, so a caller gets an error
//! that says what is missing rather than a link that fails.

mod stats;

pub use stats::{Latency, Stats};

// Not gated on the target. Two of its modules are arithmetic over the bytes the
// host sends and reach no platform call at all, so they are compiled and tested
// wherever the workspace is, rather than only where a phone is attached.
mod android;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
mod unsupported;

#[cfg(target_os = "android")]
pub use android::{Error, VideoTexture};
#[cfg(target_os = "linux")]
pub use linux::{Error, VideoTexture};
#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub use unsupported::{Error, VideoTexture};
