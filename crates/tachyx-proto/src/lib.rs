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
//! Everything else travels on the reliable control stream as ordinary Rust types
//! serialised with `postcard`. That encoding is not self-describing, so
//! [`VERSION`] is checked before anything else is read and a mismatch is reported
//! rather than misparsed.

pub mod control;
pub mod input;
pub mod media;
pub mod packet;

/// The protocol version both peers must agree on.
///
/// A client and a host are updated independently, so the first message of a
/// session carries this and a mismatch ends the session with a clear reason.
pub const VERSION: u16 = 0;
