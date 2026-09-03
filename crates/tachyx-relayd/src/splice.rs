//! Forwarding opaque datagrams between two peers, below QUIC.
//!
//! A splice sits under the session's own QUIC, on the same UDP port the control
//! connections use, separated by the one bit RFC 9000 reserves. The relay
//! rewrites the token and forwards the rest untouched: no second QUIC
//! connection, no second TLS handshake, no second congestion controller. Nine
//! bytes is the entire cost of being relayed.
//!
//! The relay is therefore addressed by a token rather than by an authenticated
//! connection, and three rules stand in for what the connection would have
//! given.
//!
//! - A token the relay does not hold is dropped without an answer, so there is
//!   nothing to probe against and a forged one is a chance in 2⁶⁴.
//! - A token is bound to one address, so a token overheard on the path cannot be
//!   replayed from elsewhere.
//! - **The relay never forwards more bytes toward an address than it has
//!   received from it** until that address has used its own token. RFC 9000
//!   gives QUIC this limit for free; a splice is below QUIC and has to state it.
//!   A peer proves its own address with an empty frame the moment it is told
//!   about the lane, which costs nine bytes and reaches nobody.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{ready, Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tachyx_relay::proto::{splice_parse, LaneToken, LANE_TOKEN_LEN};
use tachyx_relay::socket::{mapped, plain_prefix};

/// How many times a token is drawn again after it collides with a live one.
///
/// Sixty-four bits, so the first draw collides with probability under 2⁻⁴⁰ on a
/// relay holding a million lanes. This exists so the loop is bounded, not
/// because it will run.
const DRAWS: usize = 8;

/// One end of a lane.
///
/// Never derives [`std::fmt::Debug`]: the token is the key and a token in a log
/// is a splice somebody else can inject into. The counters are read through
/// [`Lanes::traffic`].
struct End {
    /// The only address this token may be used from, and where packets for this
    /// end are sent.
    ///
    /// Taken from the peer's own control connection rather than from the first
    /// packet that uses the token. The relay has to know it in order to forward
    /// anything at all, and a connection it authenticated is a better source for
    /// it than whoever sends first.
    at: SocketAddr,
    /// Whether a packet carrying this token has arrived from `at`.
    proved: bool,
    /// The other end's token, which is what a forwarded packet is rewritten to.
    counterpart: LaneToken,
    /// Bytes that arrived from this end.
    received: u64,
    /// Bytes forwarded toward it.
    sent: u64,
}

/// Where one forwarded packet goes, and under which token.
#[derive(Clone, Copy)]
struct Onward {
    to: SocketAddr,
    token: LaneToken,
}

/// Every live lane on this relay.
pub struct Lanes {
    ends: Mutex<HashMap<LaneToken, End>>,
}

/// Written by hand and says only how many. A token in a log is a splice
/// somebody else can inject into.
impl std::fmt::Debug for Lanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lanes").field("ends", &self.len()).finish()
    }
}

/// What one lane carried, for a deployment that meters or one that watches for
/// abuse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Traffic {
    /// Bytes that arrived from this end.
    pub received: u64,
    /// Bytes forwarded toward it.
    pub sent: u64,
}

impl Default for Lanes {
    fn default() -> Self {
        Self::new()
    }
}

impl Lanes {
    /// No lanes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ends: Mutex::new(HashMap::new()),
        }
    }

    /// Opens a lane between two addresses, one token per direction.
    ///
    /// Each peer is given only its own token. The relay rewrites it before
    /// forwarding, so neither peer ever learns the other's.
    pub fn open(&self, first: SocketAddr, second: SocketAddr) -> (LaneToken, LaneToken) {
        let mut ends = guard(&self.ends);
        let one = draw(&ends);
        let two = draw(&ends);
        ends.insert(
            one,
            End {
                at: first,
                proved: false,
                counterpart: two,
                received: 0,
                sent: 0,
            },
        );
        ends.insert(
            two,
            End {
                at: second,
                proved: false,
                counterpart: one,
                received: 0,
                sent: 0,
            },
        );
        (one, two)
    }

    /// Closes a lane and its counterpart.
    ///
    /// A lane dies with either control connection, so this is called from
    /// whichever ends first.
    pub fn close(&self, token: LaneToken) {
        let mut ends = guard(&self.ends);
        if let Some(end) = ends.remove(&token) {
            ends.remove(&end.counterpart);
        }
    }

    /// Follows a peer whose address moved, which is what a NAT rebinding looks
    /// like from here.
    pub fn repoint(&self, token: LaneToken, at: SocketAddr) {
        if let Some(end) = guard(&self.ends).get_mut(&token) {
            end.at = at;
            // A new address has proved nothing. The peer's next packet through
            // the lane proves it again, and until then the byte limit stands.
            end.proved = false;
        }
    }

    /// What this end of a lane has carried.
    #[must_use]
    pub fn traffic(&self, token: LaneToken) -> Option<Traffic> {
        guard(&self.ends).get(&token).map(|end| Traffic {
            received: end.received,
            sent: end.sent,
        })
    }

    /// Bytes this relay has taken in over every live lane.
    ///
    /// The number a metered deployment would bill, and the number an unmetered
    /// one watches for abuse.
    #[must_use]
    pub fn carried(&self) -> u64 {
        guard(&self.ends)
            .values()
            .fold(0u64, |total, end| total.saturating_add(end.received))
    }

    /// How many lane ends are live, which is twice the number of splices.
    #[must_use]
    pub fn len(&self) -> usize {
        guard(&self.ends).len()
    }

    /// Whether nothing is spliced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Decides one packet: where it goes, or nowhere.
    fn forward(&self, from: SocketAddr, token: LaneToken, bytes: usize) -> Option<Onward> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let mut ends = guard(&self.ends);
        let end = ends.get_mut(&token)?;
        if end.at != from {
            // The right token from the wrong address. Dropped, so a token seen
            // on the path cannot be replayed from anywhere else.
            return None;
        }
        end.proved = true;
        end.received = end.received.saturating_add(bytes);
        let counterpart = end.counterpart;
        if bytes == 0 {
            // The probe that proves this address, and nothing else. Forwarding
            // it would put an empty datagram on the far side for no reason.
            return None;
        }
        let other = ends.get_mut(&counterpart)?;
        if !other.proved && other.sent.saturating_add(bytes) > other.received {
            // Nothing has arrived from that address, so nothing is sent to it.
            return None;
        }
        other.sent = other.sent.saturating_add(bytes);
        Some(Onward {
            to: other.at,
            token: counterpart,
        })
    }
}

/// A token no live lane holds.
fn draw(ends: &HashMap<LaneToken, End>) -> LaneToken {
    std::iter::repeat_with(random_token)
        .take(DRAWS)
        .find(|drawn| !ends.contains_key(drawn))
        .unwrap_or_else(random_token)
}

/// Eight bytes from the system's random source.
fn random_token() -> LaneToken {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0u8; LANE_TOKEN_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system random source is readable");
    LaneToken::from_bytes(bytes)
}

/// The lanes, whether or not a thread died holding them.
fn guard<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The relay's own socket: quinn's, with the spliced traffic taken out first.
///
/// The two kinds of packet share one UDP port because a peer has exactly one
/// socket and the address the relay observes must be the address that peer is
/// reached at. Telling them apart is one comparison on the first byte.
#[derive(Debug)]
pub struct SpliceSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    lanes: Arc<Lanes>,
    /// Whether the socket underneath is IPv6, so a forwarded packet is
    /// addressed in a form it can send to.
    ipv6: bool,
}

impl SpliceSocket {
    /// Wraps a bound socket.
    ///
    /// # Errors
    /// If the runtime will not take the socket.
    pub fn new(socket: std::net::UdpSocket, lanes: Arc<Lanes>) -> io::Result<Arc<Self>> {
        use quinn::Runtime as _;

        let ipv6 = socket.local_addr()?.is_ipv6();
        Ok(Arc::new(Self {
            inner: quinn::TokioRuntime.wrap_udp_socket(socket)?,
            lanes,
            ipv6,
        }))
    }

    /// Forwards a batch of spliced packets, and says whether it was one.
    ///
    /// A `RecvMeta` can describe several datagrams the kernel coalesced. They
    /// share a source, so they are spliced together or not at all, unless a
    /// control connection put a packet of exactly the segment size in the same
    /// burst. Only the leading run that agrees is acted on, which is the same
    /// rule the peer's own socket applies.
    fn forward(&self, buffer: &mut [u8], meta: &mut RecvMeta) -> bool {
        let stride = if meta.stride == 0 {
            meta.len
        } else {
            meta.stride
        };
        if stride == 0 || splice_parse(&buffer[..stride.min(meta.len)]).is_none() {
            meta.len = plain_prefix(buffer, meta.len, stride.max(1));
            return false;
        }
        let from = canonical(meta.addr);
        for start in (0..meta.len).step_by(stride) {
            let end = (start + stride).min(meta.len);
            let Some((token, payload)) = splice_parse(&buffer[start..end]) else {
                break;
            };
            let carried = payload.len();
            let Some(onward) = self.lanes.forward(from, token, carried) else {
                continue;
            };
            // The token is rewritten where it lies, so a forwarded packet is
            // never copied and never allocated for.
            buffer[start + 1..start + 1 + LANE_TOKEN_LEN].copy_from_slice(onward.token.as_bytes());
            let _ = self.inner.try_send(&Transmit {
                destination: mapped(onward.to, self.ipv6),
                ecn: None,
                contents: &buffer[start..end],
                segment_size: None,
                src_ip: None,
            });
        }
        true
    }
}

impl AsyncUdpSocket for SpliceSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let filled = ready!(self.inner.poll_recv(cx, buffers, meta))?;
        let mut kept = 0;
        for index in 0..filled {
            if self.forward(&mut buffers[index], &mut meta[index]) {
                continue;
            }
            if kept != index {
                buffers.swap(kept, index);
                meta.swap(kept, index);
            }
            kept += 1;
        }
        // Zero is a whole batch of spliced packets and not an error. quinn polls
        // again, and the socket runs dry the way it always does.
        //
        // quinn's own work limiter counts the datagrams it was handed, so a poll
        // that hands it none counts as no work and the driver keeps polling. The
        // loop is therefore bounded by the kernel receive buffer rather than by
        // that limiter: under a heavy splice the driver drains the socket before
        // it yields. Four MiB of buffer is a few thousand datagrams, which is a
        // few milliseconds.
        Poll::Ready(Ok(kept))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    /// One datagram a transmit.
    ///
    /// A forwarded packet is sent one at a time, and the control connections
    /// carry a few hundred bytes a minute, so there is nothing here for a
    /// segmented send to save.
    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// The same address, with an IPv4-mapped IPv6 one written as the IPv4 it is.
///
/// An endpoint bound to IPv6 reports a v4 peer as `::ffff:a.b.c.d`, so a table
/// keyed on the address as written would miss half its own entries.
#[must_use]
pub fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, last], 47_989))
    }

    #[test]
    fn a_lane_draws_a_different_token_for_each_direction() {
        let lanes = Lanes::new();
        let (one, two) = lanes.open(address(1), address(2));
        assert_ne!(one, two, "a peer must never be given the other's token");
        assert_eq!(lanes.len(), 2);
    }

    #[test]
    fn nothing_is_forwarded_toward_an_address_that_has_not_used_its_own_token() {
        let lanes = Lanes::new();
        let (client, host) = lanes.open(address(1), address(2));
        assert!(
            lanes.forward(address(1), client, 1200).is_none(),
            "the host has proved nothing, so a relay that forwarded here would be an amplifier"
        );
        // The empty probe, which is what a peer sends the moment it is told
        // about the lane.
        assert!(lanes.forward(address(2), host, 0).is_none());
        let onward = lanes
            .forward(address(1), client, 1200)
            .expect("the host has proved its address");
        assert_eq!(onward.to, address(2));
        assert_eq!(
            onward.token, host,
            "the token is rewritten to the far end's"
        );
    }

    #[test]
    fn a_token_used_from_another_address_is_dropped() {
        let lanes = Lanes::new();
        let (client, host) = lanes.open(address(1), address(2));
        assert!(lanes.forward(address(2), host, 0).is_none());
        assert!(
            lanes.forward(address(9), client, 1200).is_none(),
            "a token overheard on the path must not be replayable from elsewhere"
        );
        assert!(lanes.forward(address(1), client, 1200).is_some());
    }

    #[test]
    fn a_token_no_lane_holds_reaches_nothing() {
        let lanes = Lanes::new();
        lanes.open(address(1), address(2));
        let forged = LaneToken::from_bytes([0xab; LANE_TOKEN_LEN]);
        assert!(lanes.forward(address(1), forged, 1200).is_none());
    }

    #[test]
    fn a_lane_dies_with_either_end() {
        let lanes = Lanes::new();
        let (client, host) = lanes.open(address(1), address(2));
        lanes.close(client);
        assert!(lanes.is_empty(), "both ends go with the one that closed");
        assert!(lanes.forward(address(2), host, 0).is_none());
    }

    #[test]
    fn a_lane_counts_what_it_carried_in_each_direction() {
        let lanes = Lanes::new();
        let (client, host) = lanes.open(address(1), address(2));
        assert!(lanes.forward(address(2), host, 0).is_none());
        assert!(lanes.forward(address(1), client, 1200).is_some());
        assert_eq!(
            lanes.traffic(client),
            Some(Traffic {
                received: 1200,
                sent: 0
            })
        );
        assert_eq!(
            lanes.traffic(host),
            Some(Traffic {
                received: 0,
                sent: 1200
            })
        );
    }

    #[test]
    fn a_peer_that_moved_has_to_prove_its_new_address() {
        let lanes = Lanes::new();
        let (client, host) = lanes.open(address(1), address(2));
        assert!(lanes.forward(address(2), host, 0).is_none());
        assert!(lanes.forward(address(1), client, 1200).is_some());

        lanes.repoint(host, address(3));
        assert!(
            lanes.forward(address(1), client, 1200).is_none(),
            "an address the relay has heard nothing from is an address it does not send to"
        );
        assert!(lanes.forward(address(3), host, 0).is_none());
        assert_eq!(
            lanes
                .forward(address(1), client, 1200)
                .expect("proved again")
                .to,
            address(3)
        );
    }
}
