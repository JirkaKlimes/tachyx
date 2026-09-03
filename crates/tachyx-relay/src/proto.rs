//! What a peer and a relay say to each other, and how a spliced packet is
//! wrapped.
//!
//! [`spec/relay.md`](../../../spec/relay.md) is the document this implements.
//!
//! Two encodings, because there are two jobs. Control travels on one
//! bidirectional stream as `postcard`, framed the way
//! [`tachyx_transport::control`] frames the session's own control stream: a
//! `u32` big-endian length and that many bytes. Spliced traffic does not travel
//! on that connection at all; it is UDP beside it, and [`splice_frame`] is its
//! whole header.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tachyx_crypto::{DeviceId, DeviceKey, Nameplate, Signature};

use crate::RelayError;

/// The application protocol a relay announces.
pub const ALPN: &[u8] = b"tachyx-relay";

/// The port a relay listens on when nothing says otherwise.
///
/// One above the host's 47989, so a machine can run both.
pub const DEFAULT_PORT: u16 = 47_990;

/// The version both ends must agree on.
///
/// `postcard` writes no field names, so the first message in each direction
/// carries this and it is read before anything after it. A mismatch is then
/// reported by name rather than misparsed.
pub const VERSION: u16 = 0;

/// How far apart the two clocks may be before an offer is stale.
///
/// Generous, because the two clocks are not related and the only thing a replay
/// buys an attacker is one wasted punch attempt at an address that is no longer
/// there.
pub const OFFER_WINDOW: u64 = 60;

/// The most bytes either end will read in one control message.
///
/// A pairing flight is the largest thing here and it is under a kilobyte.
/// Anything near this is a peer that has lost the framing, and reading it would
/// mean allocating on a stranger's word.
pub const MAX_MESSAGE: usize = 16 * 1024;

/// What the domain separator on an offer's signature says it is.
///
/// What is actually signed is `0x0f` and then these bytes and then the body:
/// `DeviceKey::sign` puts the domain's length in front. A reimplementation from
/// the specification alone would produce signatures that do not verify.
const OFFER_DOMAIN: &[u8] = b"tachyx-offer-v1";

/// The most addresses one offer may name.
///
/// A host has a handful: a link-local address or two, one routable address, and
/// the one the relay observed. A dozen is generous and it is the difference
/// between a punch and an amplifier.
pub const MAX_CANDIDATES: usize = 12;

/// The peer's first message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHello {
    /// Read before anything after it.
    ///
    /// Private so that nothing in this workspace can build a hello claiming
    /// another version. It is not what refuses one: `Deserialize` is derived, so
    /// `postcard::from_bytes` will happily produce any version at all. The check
    /// lives in [`Hello::decode`], and that is the only way to read a hello.
    version: u16,
}

/// The relay's first message, sent before the peer's is read, so a peer this
/// build cannot talk to is told which version the relay speaks rather than left
/// with a connection that closed for no stated reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHello {
    version: u16,
}

/// A first message whose version is checked before the rest of it is read.
pub trait Hello: Sized {
    /// Announces this build's version.
    #[must_use]
    fn new() -> Self;

    /// Reads the message, checking the version first.
    ///
    /// # Errors
    /// If the version does not match this build, or the bytes are not this
    /// message.
    fn decode(bytes: &[u8]) -> Result<Self, RelayError>;
}

macro_rules! hello {
    ($name:ident) => {
        impl Hello for $name {
            fn new() -> Self {
                Self { version: VERSION }
            }

            fn decode(bytes: &[u8]) -> Result<Self, RelayError> {
                let (version, _) = postcard::take_from_bytes::<u16>(bytes)
                    .map_err(|_| RelayError::Malformed)?;
                if version != VERSION {
                    return Err(RelayError::Version { theirs: version });
                }
                postcard::from_bytes(bytes).map_err(|_| RelayError::Malformed)
            }
        }
    };
}

hello!(PeerHello);
hello!(RelayHello);

/// What a peer asks a relay for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerMessage {
    /// Be reachable under this connection's identifier. A host sends this and
    /// keeps the connection; a client that only wants to reach somebody does
    /// not have to.
    Register,
    /// Open a pairing mailbox. Answered with [`RelayMessage::Nameplate`].
    Allocate,
    /// Enter an open mailbox as its second party.
    Join {
        /// Which mailbox.
        nameplate: Nameplate,
    },
    /// For the other party of my mailbox. The relay forwards the bytes and
    /// reads none of them.
    Mail {
        /// One pairing flight, opaque here.
        body: Vec<u8>,
    },
    /// Close my mailbox now.
    Release,
    /// Reach the host the offer names.
    Connect {
        /// Signed by the peer that is asking.
        offer: Offer,
    },
    /// Reply to somebody who asked.
    Answer {
        /// Signed by the peer that is answering.
        offer: Offer,
    },
    /// Punching did not work. Open a relayed path to this peer.
    Splice {
        /// The peer to be spliced to.
        peer: DeviceId,
    },
}

/// What a relay tells a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayMessage {
    /// The address this connection was seen from.
    ///
    /// Sent unprompted as soon as the connection is up, and again whenever it
    /// changes, which is what a NAT rebinding looks like from here. This is the
    /// whole of the reflexive address machinery: there is no STUN server and
    /// there is no third-party STUN server.
    Observed {
        /// Where the relay saw this peer.
        address: SocketAddr,
    },
    /// The mailbox that was opened.
    Nameplate {
        /// Its number.
        nameplate: Nameplate,
    },
    /// The other party of the mailbox said this.
    Mail {
        /// One pairing flight, opaque here.
        body: Vec<u8>,
    },
    /// Somebody wants to reach this host.
    Offered {
        /// Theirs to verify, not the relay's.
        offer: Offer,
    },
    /// The host answered.
    Answered {
        /// Theirs to verify, not the relay's.
        offer: Offer,
    },
    /// A relayed path is open.
    Lane {
        /// A packet sent to the relay's own address that begins with
        /// [`SPLICE_TAG`] and this token reaches `peer`.
        token: LaneToken,
        /// Who is on the other end of it.
        peer: DeviceId,
    },
    /// What could not be done, and why.
    Refused {
        /// The reason, in a form a client can act on.
        what: Refusal,
    },
}

/// Why a relay would not do something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Refusal {
    /// No host is registered under that identifier. It is offline, or it has
    /// never been here.
    #[error("that host is not reachable through this relay")]
    UnknownPeer,
    /// No mailbox has that number, or it has already closed.
    #[error("that pairing code names no open mailbox")]
    UnknownNameplate,
    /// The mailbox already has its two parties.
    #[error("that pairing is already being answered by somebody else")]
    NameplateTaken,
    /// Every mailbox number is in use, which on a relay of this size means an
    /// attack rather than a busy afternoon.
    #[error("this relay has no free mailbox")]
    NoNameplates,
    /// A mailbox message from a peer that is not in one.
    #[error("this connection is not in a mailbox")]
    NotInMailbox,
    /// The offer would not verify, or is older than [`OFFER_WINDOW`].
    #[error("that offer is not signed by the device it names, or is stale")]
    BadOffer,
    /// Too much, too fast, from here.
    #[error("this relay is rate limiting this connection")]
    RateLimited,
}

/// A request to meet, signed by the peer making it.
///
/// The relay does not read it and could not verify it usefully if it did: it is
/// signed for the peer at the far end, which checks it against its own paired
/// list **before it sends one packet to any address named here**. That is what
/// the signature is for. An unsigned offer would make every relay a reflector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// Who is offering.
    pub from: DeviceId,
    /// Who it is for. Inside the signature, so an offer collected for one host
    /// cannot be replayed at another.
    pub to: DeviceId,
    /// Addresses to try, the peer's own view of itself first and what the relay
    /// observed after it.
    pub candidates: Vec<SocketAddr>,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// Ed25519 over everything above.
    pub signature: Signature,
}

/// Everything an offer's signature covers, which is everything but the
/// signature.
///
/// Built only by [`Signed::of`], which destructures an [`Offer`] exhaustively.
/// A field added to `Offer` and forgotten here would otherwise sit outside the
/// signature with no compile error and no failing test, which for a structure
/// whose whole job is to be signed is the one mistake worth making impossible.
#[derive(Serialize)]
struct Signed<'a> {
    from: DeviceId,
    to: DeviceId,
    candidates: &'a [SocketAddr],
    at: u64,
}

impl<'a> Signed<'a> {
    fn of(offer: &'a Offer) -> Self {
        let Offer {
            from,
            to,
            candidates,
            at,
            signature: _,
        } = offer;
        Self {
            from: *from,
            to: *to,
            candidates,
            at: *at,
        }
    }
}

impl Offer {
    /// Signs an offer from `key` to `to`.
    ///
    /// # Errors
    /// If the offer will not encode, which at these sizes it does.
    pub fn sign(
        key: &DeviceKey,
        to: DeviceId,
        candidates: Vec<SocketAddr>,
        at: u64,
    ) -> Result<Self, RelayError> {
        // Named field by field, so a field added to `Signed` fails to compile
        // here. `verify` reaches `Signed` through `Signed::of`, so a field added
        // to `Offer` fails to compile there. Between the two, a new field cannot
        // end up outside the signature.
        let body = postcard::to_stdvec(&Signed {
            from: key.id(),
            to,
            candidates: &candidates,
            at,
        })
        .map_err(|_| RelayError::Malformed)?;
        Ok(Self {
            from: key.id(),
            to,
            candidates,
            at,
            signature: key.sign(OFFER_DOMAIN, &body),
        })
    }

    /// Checks the signature and the clock.
    ///
    /// `now` is the reader's own clock in seconds since the Unix epoch. The
    /// window is deliberately wide: see [`OFFER_WINDOW`].
    ///
    /// # Errors
    /// If the signature is not the named device's, or the offer is outside the
    /// window.
    pub fn verify(&self, now: u64) -> Result<(), RelayError> {
        if self.at.abs_diff(now) > OFFER_WINDOW {
            return Err(RelayError::Refused(Refusal::BadOffer));
        }
        // Every candidate costs the reader a punch attempt, and a punch attempt
        // is a QUIC Initial of 1200 bytes emitted at an address the offer named.
        // The framing alone would allow about 2300 of them in one offer, which
        // turns a paired client, or a relay replaying one, into a 170-fold
        // amplifier pointed wherever it likes.
        if self.candidates.len() > MAX_CANDIDATES {
            return Err(RelayError::Refused(Refusal::BadOffer));
        }
        let body = postcard::to_stdvec(&Signed::of(self)).map_err(|_| RelayError::Malformed)?;
        self.from
            .verify(OFFER_DOMAIN, &body, &self.signature)
            .map_err(|_| RelayError::Refused(Refusal::BadOffer))
    }
}

/// What a peer puts in front of a packet it wants a relay to forward.
///
/// A relay draws two of these per splice, one per direction, and gives each peer
/// only its own. It rewrites the token before forwarding, so neither peer ever
/// learns the other's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LaneToken([u8; LANE_TOKEN_LEN]);

/// Bytes in a lane token. Sixty-four bits: a forged one is one chance in
/// 2⁶⁴ and there is nothing to probe against, because a token the relay does
/// not hold is dropped without an answer.
pub const LANE_TOKEN_LEN: usize = 8;

impl LaneToken {
    /// Takes a token from bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; LANE_TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    /// The token as its bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; LANE_TOKEN_LEN] {
        &self.0
    }
}

/// Never printed. A token in a log is a splice somebody else can inject into.
impl std::fmt::Debug for LaneToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaneToken(..)")
    }
}

/// The first byte of a spliced packet.
///
/// The top two bits are clear. RFC 9000 §17.2 requires the second bit of every
/// QUIC packet to be set, so nothing this relay forwards can be mistaken for a
/// QUIC packet of its own and nothing quinn sends can be mistaken for one of
/// these. That is what lets the relay connection and the splice share one UDP
/// port.
pub const SPLICE_TAG: u8 = 0x2a;

/// Bytes a splice costs: the tag and the token, and nothing else.
pub const SPLICE_HEADER: usize = 1 + LANE_TOKEN_LEN;

/// Writes `payload` into `out` wrapped for `token`.
///
/// `out` is cleared first, and is reused across packets so that forwarding
/// allocates nothing per packet.
pub fn splice_frame(token: LaneToken, payload: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(SPLICE_HEADER + payload.len());
    out.push(SPLICE_TAG);
    out.extend_from_slice(token.as_bytes());
    out.extend_from_slice(payload);
}

/// Reads what [`splice_frame`] wrote, and `None` for anything else.
///
/// Anything else includes every QUIC packet, which is the point: this is the one
/// test that separates the two kinds of traffic on the port.
#[must_use]
pub fn splice_parse(datagram: &[u8]) -> Option<(LaneToken, &[u8])> {
    let (&SPLICE_TAG, rest) = datagram.split_first()? else {
        return None;
    };
    let (token, payload) = rest.split_at_checked(LANE_TOKEN_LEN)?;
    Some((
        LaneToken(token.try_into().ok()?),
        payload,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::from_seed(&[seed; 32]).expect("a key")
    }

    #[test]
    fn a_hello_from_another_version_is_refused_by_name() {
        #[derive(Serialize)]
        struct Ancient {
            version: u16,
        }
        let bytes = postcard::to_stdvec(&Ancient { version: 41 }).unwrap();
        assert!(matches!(
            PeerHello::decode(&bytes),
            Err(RelayError::Version { theirs: 41 })
        ));
        assert!(PeerHello::decode(&PeerHello::new_bytes()).is_ok());
    }

    impl PeerHello {
        fn new_bytes() -> Vec<u8> {
            postcard::to_stdvec(&Self::new()).expect("a hello encodes")
        }
    }

    #[test]
    fn an_offer_verifies_against_the_device_that_signed_it() {
        let mine = key(1);
        let theirs = key(2);
        let offer = Offer::sign(&mine, theirs.id(), vec!["10.0.0.1:1".parse().unwrap()], 100)
            .unwrap();
        offer.verify(100).expect("its own signature checks out");
        offer.verify(100 + OFFER_WINDOW).expect("inside the window");
    }

    #[test]
    fn an_offer_for_one_host_cannot_be_replayed_at_another() {
        let mine = key(1);
        let mut offer = Offer::sign(&mine, key(2).id(), vec![], 100).unwrap();
        offer.to = key(3).id();
        assert!(matches!(
            offer.verify(100),
            Err(RelayError::Refused(Refusal::BadOffer))
        ));
    }

    #[test]
    fn a_tampered_candidate_list_is_refused() {
        let mine = key(1);
        let mut offer = Offer::sign(&mine, key(2).id(), vec![], 100).unwrap();
        offer.candidates.push("192.0.2.1:9".parse().unwrap());
        assert!(
            offer.verify(100).is_err(),
            "the addresses are what a reflector would rewrite"
        );
    }

    #[test]
    fn an_offer_naming_more_addresses_than_a_host_has_is_refused() {
        let mine = key(1);
        let many: Vec<SocketAddr> = (0..=u16::try_from(MAX_CANDIDATES).expect("small"))
            .map(|port| SocketAddr::from(([192, 0, 2, 1], port + 1)))
            .collect();
        let offer = Offer::sign(&mine, key(2).id(), many, 100).unwrap();
        assert!(
            matches!(
                offer.verify(100),
                Err(RelayError::Refused(Refusal::BadOffer))
            ),
            "every candidate is a 1200-byte Initial the reader emits where the offer says"
        );
    }

    #[test]
    fn a_stale_offer_is_refused_in_both_directions() {
        let offer = Offer::sign(&key(1), key(2).id(), vec![], 1_000).unwrap();
        assert!(offer.verify(1_000 + OFFER_WINDOW + 1).is_err());
        assert!(offer.verify(1_000 - OFFER_WINDOW - 1).is_err());
    }

    #[test]
    fn a_splice_frame_survives_a_round_trip_and_costs_nine_bytes() {
        let token = LaneToken::from_bytes([9u8; LANE_TOKEN_LEN]);
        let mut out = Vec::new();
        splice_frame(token, b"a quic packet", &mut out);
        assert_eq!(out.len(), SPLICE_HEADER + "a quic packet".len());
        assert_eq!(SPLICE_HEADER, 9);
        assert_eq!(splice_parse(&out), Some((token, &b"a quic packet"[..])));
    }

    #[test]
    fn nothing_a_quic_packet_can_be_reads_as_a_splice() {
        // RFC 9000 requires bit 0x40 set on every QUIC packet, and this tag
        // clears it. Every byte that could open a QUIC packet is checked.
        for first in 0x40u8..=0xffu8 {
            let datagram = [first, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            assert_eq!(splice_parse(&datagram), None, "{first:#04x}");
        }
    }

    #[test]
    fn a_frame_too_short_to_hold_a_token_is_not_one() {
        for len in 0..SPLICE_HEADER {
            let mut datagram = vec![0u8; len];
            if let Some(first) = datagram.first_mut() {
                *first = SPLICE_TAG;
            }
            assert_eq!(splice_parse(&datagram), None, "{len} bytes");
        }
    }

    #[test]
    fn a_token_is_never_printed() {
        let token = LaneToken::from_bytes([0xab; LANE_TOKEN_LEN]);
        assert_eq!(format!("{token:?}"), "LaneToken(..)");
    }
}
