//! The Linux client's decode and present path.
//!
//! An encoded H.264 access unit goes in; a Flutter texture identifier comes out,
//! and no frame byte is ever read by the CPU. GStreamer decodes on the video
//! engine, exports the decoded surface as a shared buffer, imports that buffer
//! as an OpenGL image and converts it to the one format Flutter's Linux embedder
//! accepts. The pipeline runs in the embedder's own share group, so the texture
//! Flutter draws is the texture the decoder wrote.
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
//! Everything here is Linux. On other targets the crate is empty.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{Error, Latency, Stats, VideoTexture};
