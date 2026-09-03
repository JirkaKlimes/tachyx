//! One UDP socket, and a table saying where each packet really goes.
//!
//! This is the crux of [`spec/relay.md`](../../../spec/relay.md) and the reason
//! a relayed session and a direct one are the **same** session. quinn is told a
//! **virtual address** for the peer, drawn locally from `240.0.0.0/4`. The
//! socket underneath then routes: straight out the wire for a direct path, or
//! wrapped in nine bytes and sent to a relay. quinn reads the remote address and
//! nothing else, so swapping the route moves a live session between the two with
//! no migration, no path validation and no new handshake.
//!
//! `240.0.0.0/4` because it is reserved and never routable, so a virtual address
//! can never be mistaken for somewhere to send a packet, and because it is IPv4,
//! so an endpoint bound either way can carry one. The number is drawn from a
//! counter and means nothing: the two peers never have to agree on it.
//!
//! The route is chosen once, when the session opens, and kept. The mechanism for
//! moving it is here and is exercised by the tests, because the congestion
//! controller is what is missing rather than the route swap. See *Not built yet*
//! in the specification.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::task::{ready, Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, Runtime, TokioRuntime, UdpPoller};

use crate::proto::{splice_frame, splice_parse, LaneToken, SPLICE_HEADER};
use crate::RelayError;

/// The reserved network every virtual address is drawn from.
///
/// `240.0.0.0/4`: reserved by RFC 1112 §4, never routed, and refused by every
/// stack that would otherwise try to send to it.
const VIRTUAL_NET: u32 = 0xF000_0000;

/// Addresses in that network, less the one at the very top, which is the
/// limited broadcast address.
const VIRTUAL_HOSTS: u32 = 0x1000_0000 - 1;

/// The port every virtual address wears.
///
/// It is never used to send anything. quinn compares the whole socket address,
/// and the address half is already unique, so the port is a constant rather than
/// a second counter to keep.
const VIRTUAL_PORT: u16 = 1;

/// Where a virtual address really sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Straight out the wire to this address.
    Direct(SocketAddr),
    /// Wrapped for this relay, under this lane token.
    Relayed {
        /// The relay's own address.
        relay: SocketAddr,
        /// The token this peer was given. The relay rewrites it before
        /// forwarding, so the same token identifies traffic in both
        /// directions and neither peer learns the other's.
        token: LaneToken,
    },
}

/// What each virtual address means, and how a datagram that arrives is named.
#[derive(Debug, Default)]
struct Table {
    /// Virtual address to where it really sends.
    out: HashMap<SocketAddr, Route>,
    /// A direct peer's real address to the virtual address quinn knows it by.
    inbound: HashMap<SocketAddr, SocketAddr>,
    /// A lane token to the virtual address quinn knows that peer by.
    lanes: HashMap<LaneToken, SocketAddr>,
    /// Addresses a spliced packet can arrive from. A datagram from anywhere
    /// else is never examined for a splice header.
    relays: HashSet<SocketAddr>,
}

/// The routing table under one socket.
///
/// Every change is one write under the lock, so a session moved from a relay to
/// a direct path never sees a moment with no route or two.
#[derive(Debug)]
pub struct Routes {
    table: RwLock<Table>,
    next: AtomicU32,
}

impl Default for Routes {
    fn default() -> Self {
        Self::new()
    }
}

impl Routes {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            table: RwLock::new(Table::default()),
            // From one, so the network address itself is never handed out.
            next: AtomicU32::new(1),
        }
    }

    /// Notes that spliced traffic can arrive from `relay`.
    ///
    /// Only a datagram from a noted address is examined for a splice header, so
    /// a peer that talks to no relay pays one hash lookup a datagram and nothing
    /// else.
    pub fn note_relay(&self, relay: SocketAddr) {
        write(&self.table).relays.insert(canonical(relay));
    }

    /// Forgets a relay and every lane through it.
    pub fn forget_relay(&self, relay: SocketAddr) {
        let relay = canonical(relay);
        let mut table = write(&self.table);
        table.relays.remove(&relay);
        let gone: HashSet<SocketAddr> = table
            .out
            .iter()
            .filter(|(_, route)| matches!(route, Route::Relayed { relay: at, .. } if *at == relay))
            .map(|(peer, _)| *peer)
            .collect();
        table.out.retain(|peer, _| !gone.contains(peer));
        table.lanes.retain(|_, peer| !gone.contains(peer));
    }

    /// Gives `peer` a virtual address that sends straight out the wire.
    pub fn direct(&self, peer: SocketAddr) -> SocketAddr {
        let virtual_address = self.allocate();
        self.install(virtual_address, Route::Direct(canonical(peer)));
        virtual_address
    }

    /// Gives a peer a virtual address that sends through `relay` under `token`.
    pub fn relayed(&self, relay: SocketAddr, token: LaneToken) -> SocketAddr {
        let virtual_address = self.allocate();
        self.install(
            virtual_address,
            Route::Relayed {
                relay: canonical(relay),
                token,
            },
        );
        virtual_address
    }

    /// Points an address that is already in use somewhere else.
    ///
    /// This is the upgrade, and the whole of it: quinn holds the same
    /// `Connection`, reports the same `remote_address`, and performs no path
    /// validation, because nothing it can see has changed.
    pub fn point(&self, virtual_address: SocketAddr, route: Route) {
        self.install(virtual_address, route);
    }

    /// What `virtual_address` currently means.
    #[must_use]
    pub fn route(&self, virtual_address: SocketAddr) -> Option<Route> {
        read(&self.table).out.get(&virtual_address).copied()
    }

    /// Drops a peer and everything that named it.
    pub fn forget(&self, virtual_address: SocketAddr) {
        let mut table = write(&self.table);
        table.out.remove(&virtual_address);
        table.inbound.retain(|_, held| *held != virtual_address);
        table.lanes.retain(|_, held| *held != virtual_address);
    }

    /// One write: the route, and the two ways a datagram arriving for it is
    /// recognised.
    fn install(&self, virtual_address: SocketAddr, route: Route) {
        let mut table = write(&self.table);
        table.inbound.retain(|_, held| *held != virtual_address);
        table.lanes.retain(|_, held| *held != virtual_address);
        match route {
            Route::Direct(peer) => {
                table.inbound.insert(peer, virtual_address);
            }
            Route::Relayed { relay, token } => {
                table.relays.insert(relay);
                table.lanes.insert(token, virtual_address);
            }
        }
        table.out.insert(virtual_address, route);
    }

    /// The next unused virtual address.
    fn allocate(&self) -> SocketAddr {
        let drawn = self.next.fetch_add(1, Ordering::Relaxed) % VIRTUAL_HOSTS;
        SocketAddr::new(
            Ipv4Addr::from_bits(VIRTUAL_NET | drawn).into(),
            VIRTUAL_PORT,
        )
    }
}

impl Table {
    /// Rewrites one arriving datagram so quinn sees the peer's virtual address.
    ///
    /// The address is written in the family the endpoint holds it in. quinn maps
    /// every address on an IPv6 endpoint to `::ffff:a.b.c.d`, including the
    /// virtual one it was given to dial, and a packet arriving from the bare
    /// form would look like a peer that had moved.
    ///
    /// A `RecvMeta` can describe several datagrams the kernel coalesced. They
    /// share a source, so they are spliced together or not at all, unless a
    /// relay's own control connection put a packet of exactly the segment size
    /// in the same burst. Only the leading run that agrees is kept: the rest is
    /// loss, which QUIC repairs, and the alternative is a second buffer that the
    /// batch has no room for.
    fn arrive(&self, buffer: &mut [u8], meta: &mut RecvMeta, ipv6: bool) {
        let source = canonical(meta.addr);
        if self.relays.contains(&source) {
            self.unsplice(buffer, meta, ipv6);
            return;
        }
        if let Some(&peer) = self.inbound.get(&source) {
            meta.addr = mapped(peer, ipv6);
        }
    }

    /// Strips the splice header off a batch the relay forwarded.
    fn unsplice(&self, buffer: &mut [u8], meta: &mut RecvMeta, ipv6: bool) {
        let stride = if meta.stride == 0 {
            meta.len
        } else {
            meta.stride
        };
        let first = &buffer[..stride.min(meta.len)];
        let Some((token, _)) = splice_parse(first) else {
            // A QUIC packet of the relay's own control connection.
            meta.len = plain_prefix(buffer, meta.len, stride);
            return;
        };
        let Some(&peer) = self.lanes.get(&token) else {
            // A lane this peer no longer holds. Nothing here can read it, and
            // an empty datagram is what quinn discards without comment.
            meta.len = 0;
            return;
        };
        let mut kept = 0usize;
        for start in (0..meta.len).step_by(stride) {
            let end = (start + stride).min(meta.len);
            let Some((seen, payload)) = splice_parse(&buffer[start..end]) else {
                break;
            };
            if seen != token {
                break;
            }
            let carried = payload.len();
            buffer.copy_within(start + SPLICE_HEADER..end, kept);
            kept += carried;
        }
        meta.len = kept;
        // The first segment parsed as a splice frame, so it was at least
        // `SPLICE_HEADER` bytes and so is the stride it fits in.
        meta.stride = stride - SPLICE_HEADER;
        meta.addr = mapped(peer, ipv6);
    }
}

/// How many leading bytes of a coalesced batch are not splice frames.
///
/// Public because the relay applies exactly the same rule to exactly the same
/// problem, and one implementation of it is one thing to get wrong.
#[must_use]
pub fn plain_prefix(buffer: &[u8], len: usize, stride: usize) -> usize {
    for start in (0..len).step_by(stride) {
        let end = (start + stride).min(len);
        if splice_parse(&buffer[start..end]).is_some() {
            return start;
        }
    }
    len
}

/// The one socket a peer sends everything from, with [`Routes`] under it.
///
/// The relay connection, the punch and the session all leave from here, which is
/// what makes the address a relay observes the address the peer will be reached
/// at.
#[derive(Debug)]
pub struct RoutingSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    routes: Arc<Routes>,
    /// Whether the socket underneath is IPv6, so a routed destination is written
    /// in a form it can send to. See [`mapped`].
    ipv6: bool,
}

impl RoutingSocket {
    /// Wraps a bound socket.
    ///
    /// The inner socket comes from the runtime rather than from tokio directly,
    /// which is what keeps GRO, ECN and the do-not-fragment bit that path
    /// discovery needs.
    ///
    /// # Errors
    /// If the runtime will not take the socket.
    pub fn new(socket: std::net::UdpSocket, routes: Arc<Routes>) -> io::Result<Arc<Self>> {
        let ipv6 = socket.local_addr()?.is_ipv6();
        Ok(Arc::new(Self {
            inner: TokioRuntime.wrap_udp_socket(socket)?,
            routes,
            ipv6,
        }))
    }

    /// The table underneath.
    #[must_use]
    pub const fn routes(&self) -> &Arc<Routes> {
        &self.routes
    }

    /// Sends one transmit to a relay, a wrapped datagram at a time.
    ///
    /// A relay reads a nine-byte header and forwards what is behind it, so a
    /// segmented transmit cannot be passed on whole: the kernel would cut it
    /// into pieces of which only the first carries a header. Each datagram is
    /// therefore wrapped and sent on its own, which is what the socket used to
    /// answer one segment to avoid, and it costs the relayed path what it
    /// already cost.
    ///
    /// Only the first datagram may report a failure back. quinn answers
    /// `WouldBlock` by waiting and offering the whole transmit again, so
    /// reporting it half way through would put the datagrams already sent on
    /// the wire twice. Dropping the rest instead is what the network does to a
    /// packet, and the connection recovers from it the same way.
    fn splice(&self, relay: SocketAddr, token: LaneToken, transmit: &Transmit<'_>) -> io::Result<()> {
        let destination = mapped(relay, self.ipv6);
        // An unsegmented transmit is one datagram, which is the same walk over
        // one chunk. Never zero: `chunks` panics on that.
        let segment = transmit
            .segment_size
            .unwrap_or(transmit.contents.len())
            .max(1);
        WRAPPED.with_borrow_mut(|buffer| {
            for (index, datagram) in transmit.contents.chunks(segment).enumerate() {
                splice_frame(token, datagram, buffer);
                let sent = self.inner.try_send(&Transmit {
                    destination,
                    ecn: transmit.ecn,
                    contents: buffer,
                    segment_size: None,
                    src_ip: None,
                });
                if let Err(error) = sent {
                    return if index == 0 { Err(error) } else { Ok(()) };
                }
            }
            Ok(())
        })
    }

    /// Sends an empty packet through `token`'s lane.
    ///
    /// Nine bytes and no payload: the far end's quinn discards a datagram with
    /// nothing in it, and the relay learns the address this token is used from.
    /// That is what lifts the relay's anti-amplification limit on this peer, so
    /// it goes out the moment a lane is announced and before anything is dialled
    /// through it.
    ///
    /// A failure here is not reported. The relay's own control connection is
    /// what would notice a relay that has gone, and a probe is repeated by
    /// whatever the session sends next.
    pub fn probe(&self, relay: SocketAddr, token: LaneToken) {
        WRAPPED.with_borrow_mut(|buffer| {
            splice_frame(token, &[], buffer);
            let _ = self.inner.try_send(&Transmit {
                destination: mapped(relay, self.ipv6),
                ecn: None,
                contents: buffer,
                segment_size: None,
                src_ip: None,
            });
        });
    }
}

thread_local! {
    /// Where a packet is wrapped before it goes to a relay.
    ///
    /// One buffer per thread, because `try_send` is called from the endpoint
    /// driver and from every connection at once and nine bytes of header must
    /// not cost an allocation on the video path.
    static WRAPPED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl AsyncUdpSocket for RoutingSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // The real poller, not a stub. quinn calls this after `try_send` says
        // `WouldBlock` and waits on it; a poller that answered ready at once
        // turned a 16 MB burst into a timed-out connection.
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let sent = match self.routes.route(canonical(transmit.destination)) {
            // A plain session on a local network, which never involved a relay.
            None => self.inner.try_send(transmit),
            Some(Route::Direct(peer)) => self.inner.try_send(&Transmit {
                destination: mapped(peer, self.ipv6),
                // The source address quinn recorded belongs to whatever path
                // this peer was last reached on, and may be of the other family.
                // Letting the kernel choose is the only answer that cannot fail
                // here.
                src_ip: None,
                ..transmit.clone()
            }),
            Some(Route::Relayed { relay, token }) => self.splice(relay, token, transmit),
        };
        // Nothing but `WouldBlock` may leave this function. quinn propagates any
        // other error out of the connection driver and kills the connection, so
        // a packet that cannot be sent is dropped the way the network drops one
        // and the session survives to retransmit it.
        match sent {
            Err(error) if error.kind() != io::ErrorKind::WouldBlock => {
                tracing::debug!(%error, "a packet was dropped rather than reported");
                Ok(())
            }
            outcome => outcome,
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // The whole batch, not one datagram a call: the inner socket fills every
        // buffer it was given and each is then named separately.
        let filled = ready!(self.inner.poll_recv(cx, buffers, meta))?;
        let table = read(&self.routes.table);
        for (buffer, meta) in buffers.iter_mut().zip(meta.iter_mut()).take(filled) {
            table.arrive(buffer, meta, self.ipv6);
        }
        Poll::Ready(Ok(filled))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Whatever the kernel will segment.
    ///
    /// The trait asks the socket and not the destination, and this socket
    /// carries both kinds of traffic, so answering one for the sake of the
    /// relayed path turned segmentation off for the direct path as well. That
    /// is the path five sessions in six take, and it is where a burst of the
    /// pacer's whole allowance -- seventeen datagrams at 20 Mbit/s, and four
    /// times that at a raised ceiling -- is seventeen system calls where one
    /// would do.
    ///
    /// [`RoutingSocket::splice`] does the taking apart instead, where the
    /// destination is known and only the relayed path pays for it.
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    /// Whatever the kernel coalesces.
    ///
    /// This must be what the inner socket really reports: quinn sizes its
    /// receive buffers by it, and a smaller figure would have the kernel
    /// truncate a coalesced read.
    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Builds the one endpoint everything a peer does runs on.
///
/// The relay connection, the punch and the session share it, because quinn tells
/// its own connections apart by connection identifier and because the address a
/// relay observes must be the address a peer is reached at. Measured: one
/// endpoint held three connections on one UDP port at once, with the local
/// address unchanged.
///
/// `server` is the configuration a host listens with, and `None` for a client. A
/// client endpoint that carries no server configuration discards an incoming
/// Initial rather than answering it, which is exactly what the hole punch stands
/// on.
///
/// # Errors
/// If the socket cannot be bound or the endpoint cannot be built.
pub fn endpoint(
    address: SocketAddr,
    server: Option<quinn::ServerConfig>,
    routes: &Arc<Routes>,
) -> Result<(Endpoint, Arc<RoutingSocket>), RelayError> {
    // The same hygiene a plain socket gets: four MiB of buffer asked for, and
    // the expedited forwarding mark that wins the video airtime on a contended
    // access point.
    let socket = tachyx_transport::bind_socket(address)?;
    let socket = RoutingSocket::new(socket, Arc::clone(routes))
        .map_err(|error| RelayError::Transport(error.into()))?;
    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        server,
        Arc::clone(&socket) as Arc<dyn AsyncUdpSocket>,
        Arc::new(TokioRuntime),
    )
    .map_err(|error| RelayError::Transport(error.into()))?;
    Ok((endpoint, socket))
}

/// The same address, with an IPv4-mapped IPv6 one written as the IPv4 it is.
///
/// An endpoint bound to IPv6 reports a v4 peer as `::ffff:a.b.c.d`, so a table
/// keyed on the address as written would miss half its own entries. Every key in
/// here is canonical, and every destination is written back the other way by
/// [`mapped`] before it reaches a socket.
fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

/// `address`, written the way a socket of this family can send to it.
///
/// A dual-stack IPv6 socket takes an IPv4 destination only as `::ffff:a.b.c.d`;
/// handed a bare IPv4 sockaddr the kernel refuses the send outright. quinn does
/// this for the destinations it picks itself, and a destination that came out of
/// a routing table is not one of those.
#[must_use]
pub fn mapped(address: SocketAddr, ipv6: bool) -> SocketAddr {
    match (ipv6, address.ip()) {
        (true, std::net::IpAddr::V4(four)) => {
            SocketAddr::new(four.to_ipv6_mapped().into(), address.port())
        }
        _ => address,
    }
}

/// The table, whether or not a thread died holding it.
///
/// A poisoned lock here means a route change panicked part way. The routes in
/// memory are still the routes, and refusing to send every packet over it would
/// be the larger fault.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The same, for a change.
fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(byte: u8) -> LaneToken {
        LaneToken::from_bytes([byte; 8])
    }

    fn relay() -> SocketAddr {
        "203.0.113.9:47990".parse().expect("an address")
    }

    fn meta(from: SocketAddr, len: usize) -> RecvMeta {
        RecvMeta {
            addr: from,
            len,
            stride: len,
            ecn: None,
            dst_ip: None,
        }
    }

    #[test]
    fn every_virtual_address_is_in_the_reserved_range_and_none_repeats() {
        let routes = Routes::new();
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let drawn = routes.direct("10.0.0.1:1".parse().expect("an address"));
            let std::net::IpAddr::V4(address) = drawn.ip() else {
                panic!("a virtual address is IPv4 so an endpoint bound either way can carry it")
            };
            assert!(
                address.octets()[0] >= 240,
                "{address} is outside 240.0.0.0/4 and could be routed somewhere real"
            );
            assert!(seen.insert(drawn), "{drawn} was handed out twice");
        }
    }

    #[test]
    fn an_ipv4_destination_is_written_the_way_a_dual_stack_socket_can_send_to_it() {
        let four: SocketAddr = "198.51.100.7:47989".parse().expect("an address");
        let six: SocketAddr = "[2001:db8::1]:47989".parse().expect("an address");
        assert_eq!(
            mapped(four, true),
            "[::ffff:198.51.100.7]:47989".parse().expect("an address"),
            "a bare IPv4 sockaddr on an IPv6 socket is refused by the kernel"
        );
        assert_eq!(mapped(four, false), four);
        assert_eq!(mapped(six, true), six);
        assert_eq!(
            canonical(mapped(four, true)),
            four,
            "the table's own key survives the round trip"
        );
    }

    /// The socket reports whatever the kernel will segment, so quinn builds
    /// segmented transmits for the direct path. A relay cannot take one: it
    /// reads a nine-byte header and forwards what is behind it, so a segmented
    /// transmit passed on whole would arrive as pieces of which only the first
    /// carried a header.
    #[tokio::test]
    async fn a_segmented_transmit_reaches_a_relay_as_separate_wrapped_datagrams() {
        let listening = std::net::UdpSocket::bind("127.0.0.1:0").expect("a relay to send to");
        listening
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("a timeout");
        let relay = listening.local_addr().expect("a bound address");

        let routes = Arc::new(Routes::new());
        let sending = std::net::UdpSocket::bind("127.0.0.1:0").expect("a socket to send from");
        sending.set_nonblocking(true).expect("non-blocking");
        let socket =
            RoutingSocket::new(sending, Arc::clone(&routes)).expect("the runtime takes it");
        let peer = routes.relayed(relay, token(7));

        // The direct path must keep whatever the kernel offers. Compared
        // against a plain socket on the same machine rather than against a
        // number, because the number is the kernel's and a build without
        // `UDP_SEGMENT` correctly reports one.
        let plain = TokioRuntime
            .wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").expect("a socket"))
            .expect("the runtime takes it");
        assert_eq!(
            socket.max_transmit_segments(),
            plain.max_transmit_segments(),
            "segmentation is thrown away for every session, relayed or not"
        );

        // The dance quinn itself does: a tokio socket answers `WouldBlock`
        // until the reactor has registered an interest in writing it.
        let mut poller = Arc::clone(&socket).create_io_poller();
        std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
            .await
            .expect("the socket becomes writable");

        // Three datagrams of eight bytes in one transmit, which is what quinn
        // hands over once the burst allowance lets a picture out back to back.
        let contents = b"aaaaaaaabbbbbbbbcccccccc";
        socket
            .try_send(&Transmit {
                destination: peer,
                ecn: None,
                contents,
                segment_size: Some(8),
                src_ip: None,
            })
            .expect("loopback takes it");

        for expected in [&b"aaaaaaaa"[..], b"bbbbbbbb", b"cccccccc"] {
            let mut arrived = [0u8; 64];
            let (len, _) = listening.recv_from(&mut arrived).expect("a datagram arrived");
            let (carried, payload) = splice_parse(&arrived[..len]).expect("a spliced datagram");
            assert_eq!(carried, token(7), "every piece carries the lane token");
            assert_eq!(payload, expected);
        }
    }

    #[test]
    fn a_datagram_from_a_direct_peer_arrives_under_its_virtual_address() {
        let routes = Routes::new();
        let peer: SocketAddr = "198.51.100.7:47989".parse().expect("an address");
        let known_as = routes.direct(peer);
        let mut buffer = *b"a quic packet";
        let mut meta = meta(peer, buffer.len());
        read(&routes.table).arrive(&mut buffer, &mut meta, false);
        assert_eq!(meta.addr, known_as);
        assert_eq!(&buffer[..meta.len], b"a quic packet");
    }

    #[test]
    fn a_spliced_datagram_loses_its_nine_bytes_and_gains_the_peers_address() {
        let routes = Routes::new();
        let known_as = routes.relayed(relay(), token(1));
        let mut wrapped = Vec::new();
        splice_frame(token(1), b"a quic packet", &mut wrapped);
        let mut meta = meta(relay(), wrapped.len());
        read(&routes.table).arrive(&mut wrapped, &mut meta, false);
        assert_eq!(meta.addr, known_as);
        assert_eq!(&wrapped[..meta.len], b"a quic packet");
    }

    #[test]
    fn a_quic_packet_from_the_relay_itself_is_left_alone() {
        let routes = Routes::new();
        routes.note_relay(relay());
        // The second bit is set on every QUIC packet, which is what stops one
        // being read as a splice frame.
        let mut buffer = [0xc0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut meta = meta(relay(), buffer.len());
        read(&routes.table).arrive(&mut buffer, &mut meta, false);
        assert_eq!(meta.addr, relay(), "the relay's own connection is its own");
        assert_eq!(meta.len, 10, "nothing was stripped");
    }

    #[test]
    fn a_forged_lane_token_names_nobody_and_carries_nothing() {
        let routes = Routes::new();
        routes.relayed(relay(), token(1));
        let mut wrapped = Vec::new();
        splice_frame(token(0xff), b"a quic packet", &mut wrapped);
        let mut meta = meta(relay(), wrapped.len());
        read(&routes.table).arrive(&mut wrapped, &mut meta, false);
        assert_eq!(
            meta.len, 0,
            "a token this peer does not hold reaches nothing"
        );
    }

    #[test]
    fn a_coalesced_batch_of_one_lane_is_unwrapped_whole() {
        let routes = Routes::new();
        let known_as = routes.relayed(relay(), token(2));
        let mut batch = Vec::new();
        let mut one = Vec::new();
        for body in [&b"first-packet"[..], b"secondpacket", b"third-packet"] {
            splice_frame(token(2), body, &mut one);
            batch.extend_from_slice(&one);
        }
        let stride = SPLICE_HEADER + 12;
        let mut meta = RecvMeta {
            addr: relay(),
            len: batch.len(),
            stride,
            ecn: None,
            dst_ip: None,
        };
        read(&routes.table).arrive(&mut batch, &mut meta, false);
        assert_eq!(meta.addr, known_as);
        assert_eq!(meta.stride, 12);
        assert_eq!(&batch[..meta.len], b"first-packetsecondpacketthird-packet");
    }

    #[test]
    fn moving_a_route_from_a_relay_to_the_wire_changes_nothing_quinn_can_see() {
        let routes = Routes::new();
        let known_as = routes.relayed(relay(), token(3));
        let peer: SocketAddr = "198.51.100.7:47989".parse().expect("an address");
        routes.point(known_as, Route::Direct(peer));

        assert_eq!(routes.route(known_as), Some(Route::Direct(peer)));
        let mut wrapped = Vec::new();
        splice_frame(token(3), b"a quic packet", &mut wrapped);
        let mut through_the_relay = meta(relay(), wrapped.len());
        read(&routes.table).arrive(&mut wrapped, &mut through_the_relay, false);
        assert_eq!(
            through_the_relay.len, 0,
            "the old lane is gone with the route that named it"
        );

        let mut buffer = *b"a quic packet";
        let mut direct = meta(peer, buffer.len());
        read(&routes.table).arrive(&mut buffer, &mut direct, false);
        assert_eq!(
            direct.addr, known_as,
            "the session keeps the address it has always had"
        );
    }
}
