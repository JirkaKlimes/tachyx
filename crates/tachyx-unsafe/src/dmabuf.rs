//! Handing a shared buffer descriptor to GStreamer.
//!
//! One call, on the host's capture path. A descriptor is an integer to the type
//! system, so nothing but the signature below can state who closes it.

use std::os::fd::OwnedFd;

use gstreamer_allocators::DmaBufAllocator;
use gstreamer_allocators::prelude::*;

/// Wraps one plane of a shared buffer as GStreamer memory.
///
/// `size` is the length of the plane's mapping, not of the whole buffer.
///
/// # Errors
/// If GStreamer refuses the descriptor.
pub fn wrap_plane(
    allocator: &DmaBufAllocator,
    fd: OwnedFd,
    size: usize,
) -> Result<gstreamer::Memory, gstreamer::glib::BoolError> {
    #[expect(
        unsafe_code,
        reason = "the one call this module exists to make; the lint stays on so a second one is caught"
    )]
    // SAFETY: GStreamer takes ownership of the descriptor and closes it when the
    // memory is released. An `OwnedFd` owns its descriptor exclusively and gives
    // it up here, so nothing else can close it or use it afterwards, which is
    // exactly the obligation the unsafe marker exists to police.
    //
    // The marker is on the underlying method because its bound also admits a
    // bare integer descriptor, for which ownership cannot be established. Taking
    // `OwnedFd` by value is what discharges it, so this signature is the safety
    // proof rather than a restatement of it.
    unsafe {
        allocator.alloc_dmabuf(fd, size)
    }
}
