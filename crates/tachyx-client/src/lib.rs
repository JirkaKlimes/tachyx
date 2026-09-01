//! The tachyx client core.
//!
//! Everything the client does that is not drawing or touching the screen: the
//! session state machine, the batching that turns a touch sample rate into a
//! packet rate, and the video reassembly that `tachyx-proto` supplies.
//!
//! None of it performs I/O, and none of it frames anything. [`Session`] is
//! handed the messages that arrived and asked for the messages to send, so the
//! protocol behaviour is testable with no socket, no runtime and no clock. The
//! QUIC connection and the length prefix belong to `tachyx-transport`, the loop
//! that joins the two belongs to `tachyx-driver`, and the presentation on top
//! belongs to the Flutter application through `tachyx-client-ffi`.

#![forbid(unsafe_code)]

pub mod input;
pub mod session;

pub use session::{BITRATE_CEILING, Liveness, Session, capabilities};
