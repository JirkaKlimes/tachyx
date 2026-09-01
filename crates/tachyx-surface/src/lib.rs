//! The window behind Flutter's Android surface producer.
//!
//! Flutter hands a plugin an `android.view.Surface` and the platform decoder
//! takes an `ANativeWindow`. One foreign call turns the first into the second,
//! and it takes a raw Java object reference that no type can prove is a live
//! surface, which is why this is a crate of its own: every other crate in the
//! workspace sets `unsafe_code = "forbid"`, and a `forbid` cannot be lifted from
//! inside the crate it applies to.
//!
//! That call is the whole crate. What is left is a table of the producers the
//! Kotlin half has opened, so that the decoder can ask what surface it should be
//! writing into now rather than being handed one that has since been replaced.
//! The surface really is replaced, twice over: the producer builds a new image
//! reader whenever it is resized, and Flutter takes the surface away entirely
//! while the application is in the background.
//!
//! A test in `tachyx-proto` asserts that no other crate tries to lift the ban.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub use android::{producer, Producer, Surface};

// The three entry points the Kotlin half calls. They are named here as well as
// defined, because a symbol the Java virtual machine looks up is not reachable
// through the module tree and would otherwise read as dead.
#[cfg(target_os = "android")]
pub use android::{
    Java_com_tachyx_tachyx_VideoSurfaceNative_attach,
    Java_com_tachyx_tachyx_VideoSurfaceNative_close,
    Java_com_tachyx_tachyx_VideoSurfaceNative_detach,
};
