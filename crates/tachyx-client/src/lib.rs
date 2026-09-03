//! The tachyx client core.
//!
//! Everything the client does that is not drawing, touching the screen or
//! making a sound: the session state machine, the batching that turns a touch
//! sample rate into a packet rate, the video reassembly that `tachyx-proto`
//! supplies, and the ordering that puts the sound back the way it was captured.
//!
//! None of it performs I/O, and none of it frames anything. [`Session`] is
//! handed the messages that arrived and asked for the messages to send, so the
//! protocol behaviour is testable with no socket, no runtime and no clock. The
//! QUIC connection and the length prefix belong to `tachyx-transport`, the loop
//! that joins the two belongs to `tachyx-driver`, and the presentation on top
//! belongs to the Flutter application through `tachyx-client-ffi`.

#![forbid(unsafe_code)]

pub mod audio;
pub mod cursor;
pub mod input;
pub mod session;

pub use audio::{AudioEvent, AudioQueue};
pub use cursor::{CursorShape, CursorTracker, CursorView, Spot, Tracking};
pub use session::{BITRATE_CEILING, Liveness, Session, capabilities};
