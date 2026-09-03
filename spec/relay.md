# tachyx relay (draft 0)

Status: draft. `relayd` implements what is written here; anything not yet built
says so.

The relay does four jobs and holds no session key while doing any of them.

1. **Presence.** A host keeps one connection open so a client can find it.
2. **Rendezvous.** Mailboxes for pairing, and offer/answer for connecting.
3. **Reflexive address reporting.** It tells each peer the address it was seen
   from. This is the STUN job, done over the connection that is already there.
   There is no separate STUN server and there is no third-party STUN server.
4. **Splicing.** When hole punching fails it forwards opaque datagrams between
   two peers.

A hostile relay can drop traffic, delay it, lie about a reflexive address, and
watch who talks to whom and how much. It cannot read a session, join one, or
inject into one, because the session's own TLS runs end to end through it. That
is what makes a community relay and the paid fleet interchangeable, and it is
the property to keep.

## Reaching a relay

```
relay = "100.64.0.2:47990#GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA"
```

Address, then `#`, then the relay's `DeviceId` in base32, or a bare
`address:port` for a relay the operator chose not to pin. A relay is a device
like any other: the same Ed25519 key, the same self-signed certificate, the same
exact-key check when a key is written. The key travels in the address because a
relay address is configuration a person pastes once, and pinning on first sight
would leave that one moment undefended for no gain.

A bare address is a decision, not a default, and it is never arrived at by
accident: a `#` followed by anything that is not a key is refused rather than
read as no key, so a typo cannot quietly drop a pin. The peer that connects to
an unpinned relay says in its log what that gives up, every time it connects,
because a caveat shown once at configuration is a caveat nobody remembers.

Port 47990. The host listens on 47989.

### What the relay's key does and does not buy

It is not what makes this zero-trust, and reading it that way would be reading
this document wrong.

A relay has a key because QUIC has TLS and something must be presented. What
**verifying** it buys is one thing only: **metadata privacy.** Without the check,
anyone who can intercept the relay's address becomes the relay, learns who this
device reaches and when and how much, and can force every session onto the
relayed path by lying about the reflexive address. With the check, that attacker
can deny service and nothing else.

Everything above the metadata is safe either way. The session is end-to-end TLS
between two pinned device keys and pairing is a balanced PAKE, so a relay that is
hostile, impersonated, or simply somebody else's cannot read a session, join one,
inject into one, or turn one online guess into two. That is why a community relay
and a paid one are interchangeable, and it is the property every other decision
here is measured against.

So the check is cheap privacy, not the foundation. A deployment that does not
care who carries its ciphertext can leave the key off the address and accept
whatever answers; the sessions through it are exactly as safe. What it gives up
is knowing whose relay it is on.

## The connection

One QUIC connection, ALPN `tachyx-relay`. One bidirectional stream carries
control, framed exactly as [`protocol.md`](protocol.md) frames it: a `u32`
big-endian length and that many `postcard` bytes, first message in each
direction carrying the version.

Both ends present a certificate. The relay takes the client certificate's key as
the peer's identity and **authorises nothing**: it needs a stable name to route
by, not a decision about who may talk to whom. That decision belongs to the host,
which makes it in its own TLS handshake, and to nobody else.

Spliced traffic does **not** travel on this connection. It travels beside it, on
the same UDP port, as its own kind of packet. Why is the next section.

### One socket, two kinds of packet

A peer has exactly one UDP socket. The relay connection, the punch, and the
session all leave from it, which is what makes the address the relay observes the
address a peer will be reached at.

quinn tells its own connections apart by connection identifier, so the relay
connection and a direct session coexist on that socket with nothing to write.
Spliced traffic is separated one layer lower, by the bit QUIC reserves for
exactly this:

```
byte 0 = 0x2a   (0x40 clear, so RFC 9000 §17.2 guarantees no QUIC packet
                 can be mistaken for one of these)
bytes 1..9      the lane token
bytes 9..       the packet, verbatim
```

Nine bytes, and that is the entire cost of being relayed.

Two consequences follow from putting the routing decision under quinn, and
neither is a choice.

**No segmentation offload on the way out.** quinn asks the *socket* how many
datagrams one send may carry, not the destination, and one batch can hold
datagrams for a direct peer and a relayed one at once. The socket therefore
reports one.

**The receive side must report what the kernel actually coalesces.** quinn sizes
its receive buffers from that number, so reporting fewer than the kernel gives
truncates a coalesced read and loses data. A coalesced batch can then mix a
relay's own control packets with spliced ones; only the leading run that agrees
is kept, and the rest is loss that QUIC repairs.

## What travels through a splice, and what does not

**The session's own QUIC packets travel through it, unwrapped.** The relay
rewrites the token and forwards the rest untouched. There is no second QUIC
connection, no second TLS handshake, and no second congestion controller.

This was not the first design. Carrying the session as datagrams inside the relay
connection (QUIC in QUIC, the shape iroh's relays have) was measured and
rejected. The numbers that killed it:

- **Thirty-eight bytes of header**, against nine. An inner datagram pays the
  outer short header, the outer connection identifier, the outer packet number,
  the outer authentication tag and the DATAGRAM frame's own bound.
- **A floor, not a tax.** RFC 9000 requires Initial datagrams of 1200 bytes and
  quinn will not go below it, so an outer path with under about 1238 bytes of UDP
  payload cannot carry an inner handshake **at all**. Measured: an outer datagram
  capped at 1091 bytes completed no inner handshake, ever, because every Initial
  was silently dropped. The tailnet this is first being run on has an interface
  MTU of 1280, which leaves 1252 bytes of UDP payload. That is eleven bytes of
  margin. It is not a margin.
- **Two congestion controllers and two AEADs** on every relayed packet, with the
  outer one pacing the inner one's idea of the link.

The cost of forwarding at the UDP layer instead is that the relay is addressed by
a token rather than by an authenticated connection, and that is what the next
section is about.

### The lane token

`Splice` makes the relay draw two 8-byte tokens, one per direction, and give each
peer only its own. The relay holds `token -> (the peer that may use it, where its
counterpart is now, the counterpart's token)` and, on a packet, rewrites the token
before forwarding.

- A forged token is one chance in 2⁶⁴, and the relay drops a packet whose token
  it does not hold without answering, so there is nothing to probe against.
- A token is bound to the address that first used it. A packet arriving with the
  right token from another address is dropped, so a token overheard on the path
  cannot be replayed from elsewhere.
- **The relay never forwards more bytes toward an address than it has received
  from it** until that address has proved itself. RFC 9000 gives QUIC this
  three-to-one limit for free; a splice is below QUIC and has to state it.

  Proving it needs care, and the obvious rule deadlocks. The relay has to know
  where a peer is *before* it can forward anything to it, and in a session the
  host never speaks first, so "the address that first used the token" is an
  address that never arrives. So: a lane end's address comes from that peer's
  **control connection**, and each peer, on being told its `Lane`, sends one
  **empty spliced frame**, `0x2a` and its token and nothing else. The relay reads
  that as proof of the address and does not forward it. A peer whose control
  connection moves is repointed and has to prove the new address again.
- Tokens die with the lane, and the lane dies with either control connection.

### The peer's address never changes, so nothing migrates

Each side gives the other a **virtual address**, drawn locally from `240.0.0.0/4`
(reserved, never routable, and IPv4 so that an endpoint bound either way can
carry it). It is allocated from a counter, not derived from anything, because
the two sides never have to agree on it: it is what quinn is told, and quinn is
the only thing that reads it.

The socket underneath then routes: straight out the wire for a direct path, or
wrapped and sent to the relay. quinn is told the same remote address either way.

Measured: a session established through a relay and moved onto a direct path
mid-connection kept the same `Connection`, reported the same `remote_address`,
and performed **no path validation and no migration**. 991 Mbit/s relayed,
1200 Mbit/s direct, RTT 361 µs to 89 µs, one object throughout.

One thing that experiment also showed, and it is the sharp edge of this design:
the congestion window earned on the relayed path is wrong the instant the path
changes, because there is one controller for what are really two paths. Until
that is handled the upgrade is not switched on. See *Not built yet*.

## Messages

### Peer to relay

`PeerHello`
: The version, and nothing else. There is no role: a peer that wants to be
  reachable says so with `Register`, and a peer that does not stays quiet. A
  version mismatch is refused by name.

`Register`
: Be reachable under my identifier. A host sends this and keeps the connection.
  A client that only wants to reach somebody does not have to, which is why an
  answer cannot be routed by presence.

  Nothing acknowledges it, and `Observed` arrives *before* it is read, because
  the relay greets before it reads. So there is a window in which a host is
  connected and observed but not yet reachable, and a client asking for it in
  that window is told `UnknownPeer`. That is correct rather than a fault, and it
  is the one race a test has to wait out.

`Allocate`
: Open a pairing mailbox. Answered with `Nameplate`.

`Join { nameplate }`
: Enter an open mailbox as its second party.

`Mail { body }`
: For the other party of my mailbox. The relay forwards the bytes and reads
  none of them.

`Release`
: Close my mailbox now.

`Connect { offer }`
: Reach the host the offer names. The relay reads `offer.to` to know where to
  forward it, and reads nothing else in it: routing is the one thing it needs,
  and the signature is for the far end rather than for the relay.

`Answer { offer }`
: The host's reply. Routed by the **connection the offer arrived on**, not by
  presence: a client never sends `Register`, so there is no presence entry to
  route an answer back to.

`Splice { peer }`
: Punching did not work. Open a relayed path to that peer.

### Relay to peer

`RelayHello`
: The version.

`Observed { address }`
: The address this connection was seen from. Sent unprompted as soon as the
  connection is up, and again whenever it changes, which is what a NAT rebinding
  looks like from here.

`Nameplate { nameplate }`

`Mail { body }`

`Offered { offer }` and `Answered { offer }`
: What arrived. Who sent it is `offer.from`, inside the signature, which is where
  the reader has to check it anyway.

`Lane { token, peer }`
: A relayed path is open. A packet sent to the relay's own address that begins
  `0x2a` and this token reaches that peer.

`Refused { what }`
: What could not be done and why: `UnknownPeer`, `UnknownNameplate`,
  `NameplateTaken`, `NoNameplates`, `NotInMailbox`, `BadOffer`, `RateLimited`.
  Enough for a client to say something a person can act on, rather than
  "failed".

## The offer is signed, not sealed

```rust
pub struct Offer {
    pub from: DeviceId,
    pub to: DeviceId,
    /// Addresses to try. The peer's own view of itself first, then what the
    /// relay observed.
    pub candidates: Vec<SocketAddr>,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// Ed25519 over everything above, in `postcard`, under the domain
    /// "tachyx-offer-v1".
    pub signature: Signature,
}
```

`to` is inside the signature, so an offer collected for one host cannot be
replayed at another. What is signed is `0x0f`, then `tachyx-offer-v1`, then the
body: the signing domain travels with its own length in front, so a
reimplementation from this paragraph alone would produce signatures that do not
verify.

`at` is checked against a generous window, sixty seconds either way, because the
two clocks are not related.

Inside that window a replay is caught by the reader, not the relay: an offer
needs no nonce because its signature is already unique to its content, and a
host remembers the signatures it has answered inside the window. A repeat is
dropped after the signature and the paired-list checks, so it costs the host a
comparison and the sender nothing at all. The second bound is the **candidate
cap**. Every address in an offer costs the reader a punch
attempt, and a punch attempt is a 1200-byte QUIC Initial sent where the offer
said to send it. The message framing alone would carry about 2300 addresses,
which turns one 16 kB offer into 2.7 MB aimed wherever the signer likes. An offer
naming more than twelve is refused. A host has a link-local address or two, one
routable address, and the one the relay observed.

The host verifies the signature against its paired list **before it sends one
packet to any address the offer names.** That is what the signature is for. An
unsigned offer would make every relay a reflector: ask it to reach a host,
name the victim's address, and the host sprays QUIC Initials at the victim.

Sealing the offer to the host's key was considered and dropped. It would hide the
candidate addresses from the relay, and the relay observes the client's real
address itself, so it hides nothing the relay does not already have.

## Connecting, in order

```mermaid
sequenceDiagram
    participant C as Client
    participant R as Relay
    participant H as Host
    H->>R: Register
    R->>H: Observed
    C->>R: Connect(to H, signed offer)
    R->>H: Offered
    H->>R: Answer(signed)
    R->>C: Answered
    H--xC: punch: Initials the client drops
    C->>H: QUIC connect, mutual pinned keys
    Note over C,R,H: no answer in 2 s ⇒ Splice, and the same handshake runs inside
```

### The punch

Both ends already hold a UDP socket that the relay has seen, so the mapping a NAT
made for the relay is the mapping the peer will arrive at. Nothing else has to be
discovered.

**Only the client makes the real connection.** The host punches by starting a
connection of its own toward each of the client's candidates and abandoning it.
Its Initials open the host's NAT mapping and are then dropped: the client's
endpoint carries no server configuration, so quinn discards an incoming Initial
rather than answering it. The host cancels its throwaway attempt as soon as the
client's connection arrives, and after three seconds regardless.

That is a deliberate choice against racing two real connections and closing the
loser. Racing costs a second handshake, needs a rule for which side wins, and
leaves a window where both are live. One direction is one code path.

### When the punch fails

Two seconds without a direct connection and the client asks for a splice. The
relay draws the two tokens, tells both peers, and forwards from then on.

**The session through the splice is the same session**, in the strongest sense
available: the same `Connection`, the same ALPN, the same certificates, the same
key comparison, and the same congestion controller. Nothing about it knows it is
relayed except the socket underneath, which adds nine bytes and changes where it
sends.

#### The path is smaller and the video already knows

Nine bytes of splice header, and on a tailnet a 1280-byte interface underneath
that. The video packetiser reads `quinn::Connection::max_datagram_size` on every
frame rather than assuming a figure, so a smaller path resizes the slices with no
change anywhere.

The one figure that does have to move is `initial_mtu`, which
[`endpoint.rs`](../crates/tachyx-transport/src/endpoint.rs) sets to 1472 for a
local network. On a 1280-byte path every packet of that size is black-holed until
discovery notices, which is a stall at exactly the moment a session opens.

## Abuse

A public relay is a machine strangers can reach.

- Connections per source address are capped, on the IPv6 `/64` rather than the
  address, because a single subscriber is handed a whole one.
- A connection is judged before it costs anything. quinn hands the accept loop an
  `Incoming` before the handshake commits, which says whether the source address
  has been validated and offers `retry`, `refuse` and `ignore`. An unvalidated
  address whose bucket is empty is sent a retry; a repeat offender is ignored,
  which sends nothing at all. `max_incoming` and `incoming_buffer_size_total`
  default to 65536 and 100 MiB and are both far too generous for this.
- Control messages are rate limited per connection.
- A mailbox lives five minutes and holds one message toward the joiner. The
  opener stays connected for as long as the mailbox lives, because the mailbox
  dies with its connection.
- A nameplate is not reusable **while it is open**, and is not guessable in
  sequence: it is drawn at random from the free set, and returns to that set when
  its mailbox closes. Never reusing one at all would cap a relay at 65535
  pairings for its lifetime, and reuse is safe because the exchange is keyed on
  the nameplate *and* the secret, so a stale code cannot open a new mailbox.
- Spliced lanes are counted in bytes per peer, which is the number a metered
  deployment would bill and the number an unmetered one uses to spot abuse.
- Nothing is stored. A relay that is restarted has forgotten everything, and
  every host reconnects.

## Not built yet

Billing, blind tokens, the signed relay directory, and relay selection by measured
round trip.

**Upgrading a live relayed session onto a direct path.** The mechanism is built
and proven, and nothing in the daemon drives it. `Routes::point` swaps the route
and quinn never learns: measured on a real session, the connection reported
`240.0.0.1:1` before and after, the relay carried no further bytes, and the round
trip fell from 373 to 263 microseconds. Only the tests call it.

What is missing is not the swap. It is the congestion controller, which has one
window and one round-trip estimate for what has just become a different path. The
window a relay earned is wrong on a direct path, and the reverse is worse. Until
that is answered the route is chosen when the session opens and kept.

## What was measured and what was read

**Measured**, by building it and running it:

- One quinn endpoint holds three connections on one UDP port at once: outbound
  to a relay, outbound to a peer, and inbound from that peer, with the local address
  unchanged. Port reuse is not the obstacle to punching.
- A session over a virtual socket switched from relayed to direct mid-connection
  with no migration, no path validation and no new handshake.
- QUIC in QUIC stops working entirely below about 1238 bytes of outer UDP
  payload, rather than degrading.

**Read**, from the sources named:

- quinn 0.11 offers the application no way to move a connection to a new peer
  address. `Endpoint::rebind` changes the local socket, and server-side migration
  is passive. This is the reason the virtual address exists.
- Hole punching succeeds for roughly 70% of attempts on libp2p's published
  measurement of 4.43M tries (Trautwein et al., IMC 2026), and Tailscale reports
  over 90% of node pairs direct. The architecture's 85 to 90% is inside that range
  and is not a number this project has measured.
- The often-quoted "15 to 30% of users are behind symmetric NAT" has no primary
  source and is not used here. The best figures are Halkes and Pouwelse 2011:
  11% endpoint-dependent mapping, 79% not directly connectable.
- A NAT mapping should be refreshed at least every 55 seconds; 15 to 25 is the
  safe operating point.
