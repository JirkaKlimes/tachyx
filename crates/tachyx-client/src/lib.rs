//! The tachyx client core.
//!
//! Everything the client does that is not drawing or touching the screen: the
//! control-stream framing, the session state machine, the batching that turns a
//! touch sample rate into a packet rate, and the video reassembly that
//! `tachyx-proto` supplies.
//!
//! None of it performs I/O. [`Session`] is given the bytes that arrived and
//! asked for the bytes to write, so the protocol behaviour is testable with no
//! socket, no runtime and no clock. The QUIC connection underneath belongs to
//! `tachyx-transport`, and the presentation on top belongs to the Flutter
//! application through `tachyx-client-ffi`.

#![forbid(unsafe_code)]

pub mod framing;
pub mod input;
pub mod session;

pub use session::{Phase, Session, Update};
