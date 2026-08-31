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

## Video path

Video packets carry a fixed 16-byte header written by hand, byte by byte. At
thousands of packets a second, and as the part of the protocol most worth
auditing, the layout is specified here rather than derived from a Rust type.

All fields are little-endian.

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | `frame_id` | Reassembly key, and what a recovery request names. |
| 4 | 4 | `capture` | When the frame was captured, in units of 100 microseconds since the session began. |
| 8 | 2 | `seq` | Session-wide datagram counter, so a gap is visible across a frame boundary. Compared circularly, in the manner of RFC 1982. |
| 10 | 2 | `slice_pkts` | Packets in this slice. Every packet carries it, so any one of them can size the reassembly buffer. |
| 12 | 2 | `pkt_idx` | This packet's position in the slice. |
| 14 | 1 | `slice_id` | Which slice of the frame. |
| 15 | 1 | `flags` | Bit 0 marks a key frame, bit 1 the frame's last slice. The rest are spare and a receiver must ignore them. |

The payload follows immediately. At the 1434-byte datagram this transport reaches
on a 1500-byte path, that leaves 1418 bytes of video, or 94.5% of the wire frame.

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
