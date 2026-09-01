//! QUIC transport for tachyx.
//!
//! One connection carries everything. Video travels as datagrams, which are
//! never retransmitted; control and input travel on one bidirectional stream,
//! which is reliable and ordered. A lost video packet must not delay an input
//! event, and an input event must not delay a frame.
//!
//! The packetiser and the reassembler are not here. They are pure functions
//! over the sixteen-byte header and they live in [`tachyx_proto::packet`], so
//! that a client can rebuild video without linking a QUIC server to do it.
//!
//! Two things in here are not obvious and are the reason it exists.
//!
//! [`video::VideoSender::can_send_frame`] refuses a frame the transport would
//! otherwise accept and then shred. A successful call to send a datagram does
//! not mean the datagram was sent.
//!
//! [`identity::PinnedServer`] remembers the key a host presented the first time
//! and refuses anything else afterwards, rather than accepting whatever
//! certificate arrives.

pub mod control;
pub mod endpoint;
pub mod identity;
pub mod video;

pub use control::{ControlError, ControlReader, ControlWriter};
pub use endpoint::{Client, ClientSession, Host, HostSession, TransportError, Tuning};
pub use identity::{Fingerprint, HostIdentity, IdentityError, PinStore};
pub use video::{video_send_buffer, Sent, VideoError, VideoReceiver, VideoSender};
