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
protobuf-encoded `ControlMessage`. Protobuf is used because the client on a phone
and the daemon on a workstation are updated independently, and field numbers make
that skew survivable.

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

Specified once the packetisation design is settled.
