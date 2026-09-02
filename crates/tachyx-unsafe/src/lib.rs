//! The raw handles each platform hands across, and the only crate in the
//! workspace that may contain unsafe.
//!
//! Every other crate sets `unsafe_code = "forbid"`, and a `forbid` cannot be
//! lifted from inside the crate it applies to. A call whose obligation no type
//! can discharge therefore needs a crate boundary around it rather than a module
//! boundary. This is that crate. It holds one module per handle, and each module
//! is gated to the target that has the handle, so a build pulls in neither the
//! code nor the dependencies of a platform it is not for.
//!
//! No module here holds a decision. Each one takes a foreign handle in the type
//! that proves what the call needs, and leaves every policy to its caller.
//!
//! A test in `tachyx-proto` asserts that no other crate tries to lift the ban.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(target_os = "linux")]
pub mod glctx;
#[cfg(target_os = "android")]
pub mod surface;
