//! Reaching a peer that is not on this network.
//!
//! A relay does four jobs and holds no session key while doing any of them:
//! presence, rendezvous, telling each peer the address it was seen from, and
//! forwarding opaque packets between two peers whose hole punch did not work.
//! [`spec/relay.md`](../../../spec/relay.md) is the document, and it is the one
//! to read first.
//!
//! This crate is the protocol and the peer's half of it. The server is
//! `tachyx-relayd`.
//!
//! A hostile relay can drop traffic, delay it, lie about a reflexive address,
//! and watch who talks to whom. It cannot read a session, join one, or inject
//! into one, because the session's own TLS runs end to end through it. Keeping
//! that true is the point of every decision in here.

#![forbid(unsafe_code)]

pub mod client;
pub mod proto;
pub mod rendezvous;
pub mod socket;

pub use proto::{
    ALPN, DEFAULT_PORT, Hello, LaneToken, Offer, PeerHello, PeerMessage, Refusal, RelayHello,
    RelayMessage, SPLICE_HEADER, SPLICE_TAG, VERSION, splice_frame, splice_parse,
};
pub use client::{Lane, RelayClient, Role};
pub use rendezvous::{Arrivals, ClientRendezvous, HostRendezvous, Path};
pub use socket::{endpoint, mapped, plain_prefix, Route, Routes, RoutingSocket};

/// Why a relay could not be reached, or would not do something.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The relay refused, and said why. Its words, because a client that is told
    /// "that host is not reachable through this relay" can act on it and a
    /// client told "failed" cannot.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// The peer speaks another version of this protocol.
    #[error("the relay speaks protocol version {theirs}, this build speaks {VERSION}")]
    Version {
        /// What the far end claims.
        theirs: u16,
    },
    /// Bytes that are not a message this build reads.
    #[error("the relay sent a message this build cannot read")]
    Malformed,
    /// The socket, the endpoint or the session underneath would not work.
    #[error(transparent)]
    Transport(#[from] tachyx_transport::TransportError),
    /// The connection to the relay is not up. A host reconnects on its own; a
    /// client that wanted something is told to ask again.
    #[error("this peer is not connected to a relay")]
    Offline,
    /// The relay was asked for something and said nothing back in time.
    #[error("the relay did not answer")]
    Silent,
    /// An offer arrived that names another device, or is signed by one this
    /// host has not paired with.
    #[error("that offer is not for this device, or is from a device it has not paired with")]
    NotForUs,
}
