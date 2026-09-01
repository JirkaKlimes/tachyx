# tachyx wire protocol — draft 0

Status: draft. Covers only what the current milestone implements: one client, one
host, one LAN. Pairing, authentication and relaying are specified elsewhere when
they exist.

## Transport

A single QUIC connection carries everything.

| Path | QUIC object | Reliability |
|---|---|---|
| Video | DATAGRAM (RFC 9221) | unreliable, never retransmitted |
| Control and input | one bidirectional stream, opened by the client | reliable, ordered |

Video never shares a stream with control. A lost video packet must not delay an
input event, and an input event must not delay a frame.

### Security, currently

There is none. The host presents a self-signed certificate and the client accepts
any certificate. This is a deliberate, temporary state for latency work on a
trusted LAN. The certificate machinery is already in place so that pinning the
peer's Ed25519 key becomes a change to one verifier, not a change to the
transport.

## Control stream framing

Each message is a `u32` big-endian byte length followed by that many bytes of a
`postcard`-encoded message.

The message types are defined once, as ordinary Rust types that make invalid
states unrepresentable, and serialised with serde. Every consumer of this control
plane is Rust, so a schema compiler would buy nothing but a second set of weaker
types and the conversion layer between them.

`postcard` varint-encodes integers and lengths, needs no allocator, and has a
published wire specification. It is not self-describing, so the first message
carries the protocol version and a mismatch is reported rather than misparsed.

The video path does not use this encoding, or serde at all. See below.

## Control messages

### Client to host

`ClientHello` — first message on the stream.
: Decoders the client can drive (codec, profile, maximum level, resolution and
  frame rate, bit depth, chroma subsampling), the viewport in physical pixels,
  the device pixel ratio, and the requested resolution mode.

`ViewportChanged`
: The same capability payload, sent again. The host treats it as renegotiation.
  Sent on rotation, window resize, or when the user changes the resolution mode.

`InputBatch`
: A batch of input events, each with the client's monotonic timestamp. Batched on
  a short tick rather than sent individually, so a burst of pointer motion costs
  one packet.

`Feedback`
: The receiver report that drives congestion control. Arrival timestamps per
  received packet group, cumulative loss, and the client's current jitter buffer
  depth.

`KeyframeRequest`
: Sent when reassembly failed and the client cannot continue from what it has.

`ClockSyncRequest`
: Carries the client's send timestamp. Used to put the two clocks on a common
  basis so per-stage latency can be attributed.

### Host to client

`HostHello`
: Encoders and capture formats the host has, and the list of monitors.

`StreamConfig`
: The host's decision: codec, encoded resolution, frame rate, target bitrate, and
  the codec configuration bytes needed to initialise the decoder. Re-sent whenever
  any of those change, including a change forced by congestion.

`CursorUpdate`
: Cursor position and shape. The client renders the cursor locally so that pointer
  movement is not gated on a round trip.

`ClockSyncReply`
: The client's timestamp, the host's receive timestamp, and the host's send
  timestamp. Four timestamps in total give offset and round trip.

`HostStats`
: Per-stage timings measured on the host, for the client's statistics overlay.

## Input events

Pointer motion is relative by default, because the phone is used as a trackpad and
the finger's position on the glass has nothing to do with the pointer's position
on the remote desktop. Absolute motion exists for a desktop client whose real
mouse hovers over the video.

Keys are carried as USB HID usage codes, not characters. The host owns the
keyboard layout, so a client with a different layout still produces the key the
user pressed.

Text is carried separately as UTF-8. An on-screen keyboard on a phone produces
characters, not key presses, and no HID usage code corresponds to most of what it
can produce. The host synthesises whatever key press is needed to enter that text.

### Touch input is a trackpad, not a touch screen

The pointer does not follow the finger. The finger moves it, the way a laptop
trackpad does, so the whole screen is reachable without the hand covering it and
without the pointer jumping to wherever the user happened to touch.

Gestures are distinguished by how many fingers touch, not by how many times:

| Gesture | Effect |
|---|---|
| One-finger tap | Left click |
| Two-finger tap | Right click |
| Three-finger tap | Show or hide the on-screen keyboard |
| One-finger drag | Move the pointer |
| Two-finger drag | Scroll |
| Long press, then drag | Move the pointer with the left button held |

Counting fingers rather than taps is what makes every one of these resolve the
moment the fingers lift. Distinguishing a single tap from a double tap cannot: a
tap that has already been sent cannot be taken back, so either the click waits to
find out whether another one follows, which measured 310 milliseconds on this
framework, or a right click is always preceded by a real left click that the remote
desktop acts on. Neither is acceptable in a system built to defend a budget of ten
to twenty milliseconds.

## Constraints the client places on the encoder

Encoded width and height are multiples of sixteen. H.264 expresses a size like 1080
by coding 1088 and attaching a crop rectangle, and the Android presentation path
does not apply that rectangle. Choosing every rung of the resolution ladder from
multiples of sixteen avoids the problem rather than detecting it.

Parameter sets are repeated in the stream immediately before every key frame,
rather than sent once when the session starts. The client then needs no separate
decoder configuration step, survives a reconnection or a resolution change without
one, and recovers on its own if a parameter set is lost, which matters on a path
that never retransmits.

The sequence parameter set declares that the stream reorders no frames. A decoder
that is not told this holds frames back before emitting them, adding latency that
no client-side setting can remove.

## Video path

Video packets carry a fixed 16-byte header written by hand, byte by byte. At
thousands of packets a second, and as the part of the protocol most worth
auditing, the layout is specified here rather than derived from a Rust type.

All fields are little-endian.

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | `frame_id` | Reassembly key, and what a recovery request names. Counts encoded pictures and skips none. |
| 4 | 4 | `capture` | When the frame was captured, in units of 100 microseconds since the session began. |
| 8 | 2 | `seq` | Session-wide datagram counter, so a gap is visible across a frame boundary. Compared circularly, in the manner of RFC 1982. |
| 10 | 2 | `slice_pkts` | Packets in this slice. Every packet carries it, so any one of them can size the reassembly buffer. |
| 12 | 2 | `pkt_idx` | This packet's position in the slice. |
| 14 | 1 | `slice_id` | Which slice of the frame. |
| 15 | 1 | `flags` | Bit 0 marks a key frame, bit 1 the frame's last slice. The rest are spare and a receiver must ignore them. |

The payload follows immediately. At the 1434-byte datagram this transport reaches
on a 1500-byte path, that leaves 1418 bytes of video, or 94.5% of the wire frame.

### The frame identifier counts encoded pictures, and skips none

This is a requirement on the host, not a description of it, and the receiver
depends on it.

A frame that loses one packet announces itself: the sequence number has a gap
between two packets of the same frame, or the frame is still open when the next
frame's packets arrive. A frame that loses *every* packet announces nothing. It
leaves no trace at all except the hole it makes in the frame identifiers, so
that hole is what the receiver reads: a delivered frame whose identifier is not
one above the last one delivered means the frames in between are gone, the
reference chain is broken, and nothing may be decoded until the next key frame.

That reading is only correct if the host never skips an identifier for a reason
of its own. The rule that makes it correct is that the identifier follows the
encoder's chain of reference frames rather than the capture clock:

- A frame the host declines **before** encoding costs no identifier. The encoder
  never saw the picture, so its reference chain is unbroken, and the client has
  nothing to recover from. This is what the send gate asks about, and it is the
  cheap case.
- A frame the host declines **after** encoding — the send gate's second check,
  or any encoded picture thrown away — keeps its identifier and leaves a gap.
  The encoder's reference chain has already advanced past it, so every later
  frame refers to a picture the client will never hold. The gap is not an
  accounting artefact; it is the signal that a key frame is needed, and it is
  indistinguishable from wire loss because the consequence is identical.

Numbering by capture instead would make the two cases identical on the wire.
With one reference frame and a key frame interval of 1024 at 60 Hz, that is up
to seventeen seconds of silently wrong picture, which is why no field was added
to say which case occurred: the identifier already says it, and a second field
saying the same thing would be a second thing to get wrong.

Identifiers do not restart within a connection. A resolution change is made on a
filter without restarting the encoder, so the sequence carries across it.

The timestamp counts 100-microsecond ticks rather than microseconds because
microseconds overflow four bytes after 71 minutes, and a remote desktop session
routinely runs longer than that. At this resolution the same four bytes last 119
hours. Nothing downstream needs finer: latency is reported in milliseconds and
congestion feedback measures arrival deltas in units of 250 microseconds.

There is no field counting the slices in a frame. The last-slice flag supplies
that, which means the host does not need to know how many slices a frame will have
before it sends the first one, and can send slice zero while slice one is still
being encoded.

Slices are an encoder-level division into independently decodable units, so losing
one costs a band of the picture rather than the whole frame. Fragmentation within
a slice is a separate axis, carried by `slice_pkts` and `pkt_idx`. Fragmentation
alone would not give the property, because a lost fragment still destroys its
slice.

`slice_id` 255 and the spare flag bits are reserved for forward error correction,
which is not implemented. On a local network a retransmission costs about a
millisecond, which is cheaper than paying for parity on every frame.
