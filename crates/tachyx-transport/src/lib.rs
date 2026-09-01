//! QUIC transport for tachyx.
//!
//! One connection carries everything. Video travels as datagrams, which are
//! never retransmitted; control and input travel on one bidirectional stream,
//! which is reliable and ordered. A lost video packet must not delay an input
//! event, and an input event must not delay a frame.
//!
//! The packetiser and the reassembler are not here. They are pure functions
//! over the sixteen-byte header and they live in [`tachyx_proto::packet`], so
//! that a client can rebuild video without linking a QUIC server to do it, and
//! so that there is one implementation of turning datagrams into pictures
//! rather than one on each side of the wire.
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
//!
//! [`pace::Pacer`] releases a picture's datagrams at a byte rate rather than
//! handing them all to quinn at once. quinn's own pacer applies to datagrams
//! but never engages at these rates, and the burst it lets through is what
//! overruns the buffer at the far end.

pub mod control;
pub mod endpoint;
pub mod identity;
pub mod pace;
pub mod video;

pub use control::{ControlError, ControlReader, ControlWriter};
pub use endpoint::{Client, ClientSession, Host, HostSession, TransportError, Tuning};
pub use identity::{Fingerprint, HostIdentity, IdentityError, PinStore};
pub use pace::{PACE_PERCENT, Pacer, rate_for};
pub use video::{Sent, VideoError, VideoSender, video_send_buffer};
