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
//! Three things in here are not obvious and are the reason it exists.
//!
//! [`video::VideoSender::can_send_frame`] refuses a frame the transport would
//! otherwise accept and then shred. A successful call to send a datagram does
//! not mean the datagram was sent.
//!
//! [`audio::AudioSender`] sends past the pacer rather than through it, while
//! sharing its sequence numbers and its release records. Sound queued behind a
//! paced key frame arrives tens of milliseconds late, and a gap in sound is a
//! click that everybody hears.
//!
//! [`identity`] authenticates **both** ends. The host refuses a device it has
//! not paired with, inside the handshake; the client accepts exactly the host it
//! was told to expect. There is no trust on first use and no first-sight write.
//!
//! [`pace::Pacer`] releases a picture's datagrams at a byte rate rather than
//! handing them all to quinn at once. quinn's own pacer applies to datagrams
//! but never engages at these rates, and the burst it lets through is what
//! overruns the buffer at the far end.

pub mod audio;
pub mod control;
pub mod endpoint;
pub mod identity;
pub mod pace;
mod queue;
pub mod video;

pub use audio::{AudioError, AudioSender};
pub use control::{ControlError, ControlReader, ControlWriter};
pub use endpoint::{
    Client, ClientSession, Host, HostSession, LAN_MTU, RELAYED_MTU, TransportError, Tuning,
    bind_any, bind_socket, client_config, server_config,
};
pub use identity::{AnyDevice, Paired, Pinned, SERVER_NAME, peer_device, provider};
pub use pace::{PACE_PERCENT, Pacer, rate_for};
pub use video::{Sent, VideoError, VideoSender, video_send_buffer};
