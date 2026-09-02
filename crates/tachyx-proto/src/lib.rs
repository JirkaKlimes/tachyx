//! The tachyx wire protocol.
//!
//! Two encodings live here, because there are two jobs.
//!
//! [`media`] describes the video datagram header: sixteen bytes, packed by hand,
//! on the path that carries thousands of packets a second.
//!
//! [`packet`] turns encoded pictures into those datagrams and back again. It
//! lives here rather than in the transport so that a client can reassemble
//! video without linking a QUIC server to do it.
//!
//! [`report`] is the other half of that sequence accounting: when each datagram
//! arrived, which is the only measurement of the path the host can have.
//!
//! Everything else travels on the reliable control stream as ordinary Rust types
//! serialised with `postcard`. That encoding is not self-describing, so
//! [`VERSION`] is checked before anything else is read and a mismatch is reported
//! rather than misparsed.

pub mod control;
pub mod input;
pub mod media;
pub mod packet;
pub mod report;

/// The protocol version both peers must agree on.
///
/// A client and a host are updated independently, so the first message of a
/// session carries this and a mismatch ends the session with a clear reason.
///
/// Raised to 1 by forward error correction. A host that carries parity sends
/// datagrams with `slice_id` 255, which draft 0 reserved but did not define: a
/// draft 0 client reads one as a slice of the picture, waits for a slice 255
/// that never fills, and completes no frame at all. That is exactly the failure
/// this field exists to turn into a named refusal.
///
/// Raised to 2 by codec negotiation. [`control::Capabilities`] gained the set of
/// codecs the client decodes, and `postcard` writes no field names: a version 1
/// hello read as a version 2 one ends a field short, and a version 2 hello read
/// as a version 1 one leaves a byte over. Neither fails to parse on its own.
pub const VERSION: u16 = 2;
