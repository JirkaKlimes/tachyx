//! Adopting the OpenGL context that Flutter's Linux embedder rasterises with.
//!
//! The embedder has no interface for handing a plugin its graphics context, and
//! no proposal to add one. It does not need one. The callback that supplies a
//! texture runs on the raster thread with the embedder's own EGL context
//! already current, so the context can be read out of EGL's per-thread state
//! where it stands.
//!
//! That read is the whole crate. Two foreign calls take the display and the
//! context, and two GStreamer constructors describe them to the pipeline. All
//! four take a raw handle that no type can prove is live, which is why this is a
//! crate of its own: every other crate in the workspace sets
//! `unsafe_code = "forbid"`, and a `forbid` cannot be lifted from inside the
//! crate it applies to.
//!
//! A test in `tachyx-proto` asserts that no other crate tries to lift the ban.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{AdoptedContext, Error};
