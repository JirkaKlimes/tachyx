# tachyx wire protocol

Status: draft. Protocol version 4 (`tachyx_proto::VERSION`).

This document specifies the session: what a client and a host send each other
after they connect. [`pairing.md`](pairing.md) specifies how two devices meet.
[`relay.md`](relay.md) specifies how they reach each other across networks. A
relayed session is the same session: the relay forwards packets it cannot read.

## Versions

Each version changes bytes that the previous version misreads without a parse
error. The [version check](#control-stream-framing) turns each case into a named
refusal.

| Version | Change | What the previous version does with it |
|---|---|---|
| 1 | `slice_id` 255 marks a parity packet. Version 0 reserved it. | Reads parity as slice 255, waits for that slice to fill, and completes no frame. |
| 2 | `Capabilities` gains `decode`. | `postcard` writes no field names. A version 1 hello read as version 2 ends a field short, and the reverse leaves a byte over. Neither fails to parse. |
| 3 | `CursorReport` moves to logical pixels. Pointer pictures gain an identifier. | Misplaces the pointer by up to a fifth of the screen and draws it at the wrong size. |
| 4 | Audio datagrams (flag bit 3). `HostHello` gains `audio`. | Ignores the spare bit, reads an audio datagram as a picture packet with no fragments, rejects it, and never records its `seq`. A hundred audio packets a second then read as a hundred lost datagrams a second, which drives parity to its ceiling and the bitrate to its floor. Nothing in the encoding names the new hello field. |

## Transport

One QUIC connection carries everything.

| Path | QUIC object | Reliability |
|---|---|---|
| Video and audio | DATAGRAM frames (RFC 9221) | Unreliable. Never retransmitted. |
| Control and input | One bidirectional stream, opened by the client | Reliable, ordered. |

Video never shares a stream with control. A lost video packet must not delay an
input event, and an input event must not delay a frame.

Both ends use TLS 1.3 with ALPN `tachyx`. A connection allows one bidirectional
stream and no unidirectional streams. The idle timeout is 5 s.

### Security

Each device has one Ed25519 key pair. The public key **is** the device
identifier. There is no certificate authority. Each end presents a self-signed
certificate that carries its key, and each end checks the exact key of the
other. [`pairing.md`](pairing.md) specifies this. For a session:

- The client accepts only the key of the host it paired with.
- The host requires a client certificate. Inside the TLS handshake, it refuses a
  device that is not in its paired list, before that device can send a message.
  It reads the list during the handshake, so a revocation applies to the next
  connection.
- Neither end examines validity dates or subject names. The certificate is
  self-signed, so a forger can write anything into them. The key is the only
  thing an attacker must break.
- A certificate whose key is not Ed25519 is refused before comparison.

TLS 1.3 sends the client certificate in the client's second flight, so an
unpaired client learns of the refusal on its first read, as a TLS alert.

An earlier design trusted the host key on first use (TOFU), and the host
accepted any client that reached the port. Pairing replaced both. Neither end
pins a key on first sight.

The identity stays in the handshake. No message below names either end. A
client's permissions belong to the device that connected, and the host reads
them when the session opens ([`pairing.md`](pairing.md)).

## Control stream framing

Each message is a `u32` big-endian byte length, then that many bytes of a
`postcard`-encoded message. Each end refuses a message longer than 64 KiB
(`tachyx_transport::control::MAX_MESSAGE`).

The messages are Rust types that make invalid states unrepresentable,
serialised with serde. Every consumer of the control plane is Rust, so a schema
compiler would add only a second, weaker set of types and a conversion layer.

`postcard` varint-encodes integers and lengths. It writes an enum variant as its
index, so variant order is the wire encoding and new variants go at the end. It
is not self-describing, so the version comes first:

- The first message in each direction (`ClientHello`, `HostHello`) carries the
  protocol version as its first field.
- Each end reads that field before it decodes anything after it. A mismatch is
  reported by name, not misparsed.
- Both directions check. A peer that does not check decodes a reshaped message
  into wrong values, not into a parse error.

The client opens the stream and writes `ClientHello`. The host writes
`HostHello` before it reads the client's hello, so arrival order never matters.
A host that refuses a hello closes the connection with application code 1 and
the reason attached, for example
`peer speaks protocol version 7, this build speaks 4`. The client reports
`the host refused the connection: <reason>`. A QUIC close discards stream data
the peer has not acknowledged, so a refused client usually reads the close
reason and not the host's hello.

The datagram path does not use `postcard` or serde. See
[Datagram path](#datagram-path).

## Control messages

### Client to host

```rust
pub struct ClientHello {
    version: u16,
    pub capabilities: Capabilities,
}

pub struct Capabilities {
    pub viewport: Size,              // video surface, physical pixels
    pub max_decode: Size,            // largest picture the decoder accepts
    pub max_decode_fps: NonZeroU16,  // highest rate at max_decode
    pub requested_fps: NonZeroU16,   // rate wanted, may be lower
    pub bitrate_ceiling: NonZeroU32, // bits per second
    pub decode: Codecs,              // codecs the client decodes
}

pub enum ClientMessage {
    ViewportChanged(Capabilities),
    Input(InputBatch),
    KeyframeRequest { after: Option<FrameId> },
    Feedback(Feedback),
    Audio(AudioCommand),
}
```

`Size` is `width` then `height`, both `NonZeroU16`.

`ViewportChanged` repeats `Capabilities` after a rotation, a resize, a move into
or out of picture-in-picture, or a change to the bitrate cap. The host
renegotiates from the new viewport and `bitrate_ceiling`. It ignores `decode`
(see [Codec negotiation](#codec-negotiation)).

`Input` carries an `InputBatch`: events in order, each a `TimedInput` with the
client's session clock (`Timestamp`) at the moment of the action. See
[Input events](#input-events). The client merges consecutive events of one
continuous motion and sends at most one batch every 4 ms. The first event after
a quiet period goes out at once.

`KeyframeRequest` asks for a key frame when the decoder cannot continue from
what it holds. `after` is the last frame the client decoded, or `None`. The host
draws every key frame from one budget and can refuse a request without a reply,
so the client repeats the request, at most every 250 ms, while its decoder is
stuck.

`Feedback` is the receiver report (below, and [`congestion.md`](congestion.md)).

`Audio` controls the host's sound (see [Sound](#sound)). The client sends it
only to a host whose `HostHello` has `audio: true`. A host without audio ignores
it.

### Codec negotiation

`Codecs` is a `u8` bit set: bit 0 is H.264, bit 1 is AV1. Each end probes the
machine it runs on, not its platform: the client for decoders, the host for
encoders. The host encodes the best codec in the intersection, in
`Codec::PREFERENCE` order (AV1, then H.264), and names it in
`StreamConfig::codec`. The order is in the protocol crate, so both ends compute
the same answer. No codec in common ends the session: there is no picture to
send. H.264 is in both sets on every platform either end ships for, so the
fallback is a real path.

The set is a bit set, not a list of `Codec`. `postcard` refuses an unknown enum
discriminant, so a list that carried a codec from a later version would fail to
decode and take the whole hello with it. An unknown bit is never in an
intersection.

The host reads `decode` at the hello only, and the codec never changes during a
session. A client builds its decoder once and cannot rebuild it without losing
its reference pictures. Every `ViewportChanged` repeats the same set, and the
host must not read a change into it.

### The receiver report

```rust
pub struct Feedback {
    pub base: Seq,                   // seq of the first datagram covered
    pub reference: u32,              // client clock at the first arrival
    pub packets: Vec<PacketArrival>, // one per seq from base up, no gaps
    pub lost: u64,                   // never seen, since the session began
    pub received: u64,               // received, since the session began
}

pub enum PacketArrival {
    Missing,       // not received when the report was built
    Received(i16), // ticks after the previous arrival in this report
}
```

Times are 250-microsecond ticks (`FEEDBACK_TICK`). `reference` counts from the
start of the client's session. The first arrival is measured from `reference`,
so its delta is zero. A negative delta means the two datagrams were reordered.
In a report in which nothing arrived, `reference` is the client's clock at build
time, and no estimator reads it.

- The key is the `seq` in the media header, so no new identifier is necessary.
- Deltas, not offsets: `postcard` varint-encodes, and a delta between
  consecutive datagrams is almost always one byte.
- A signed 16-bit tick count spans 8.19 s, thirty times the longest report
  interval.
- `lost` and `received` are cumulative, so a delayed report is still exact about
  the interval it covers.

| Constant | Value | Rule |
|---|---|---|
| `FEEDBACK_INTERVAL` | 50 ms | Report interval while datagrams arrive. |
| `FEEDBACK_SLOW_INTERVAL` | 250 ms | Report interval while `StreamConfig::bitrate` is below `FEEDBACK_SLOW_BELOW`. |
| `FEEDBACK_SLOW_BELOW` | 250 kbit/s | |
| `FEEDBACK_MAX_STATUSES` | 512 | Most entries in one report. The client splits a longer report and sends the parts back to back. |

Measured with real encodings, a report of 88 datagrams (50 ms at 20 Mbit/s) is
192 bytes. At twenty reports a second that is about 0.15% of the 20 Mbit/s it
describes.

No clock synchronisation is necessary. Every quantity the host's estimator
reads is a difference between two client arrival times, so a constant offset
between the two clocks cancels.

### Host to client

```rust
pub struct HostHello {
    version: u16,
    pub audio: bool, // the host can stream its own sound
}

pub enum HostMessage {
    Stream(StreamConfig),
    Heartbeat(Heartbeat),
    Cursor(CursorUpdate),
    Audio(AudioStatus),
}

pub struct StreamConfig {
    pub codec: Codec,
    pub coded: CodedSize,       // size the encoder codes, multiples of 16
    pub visible: Size,          // part of the coded picture to show
    pub display_aspect: Aspect, // shape to draw it at, width over height
    pub framerate: NonZeroU16,  // frame rate declared to the encoder
    pub bitrate: NonZeroU32,    // target, bits per second
}

pub struct Heartbeat {
    pub sent: Timestamp,             // host session clock at send
    pub last_frame: Option<FrameId>, // newest frame put on the wire
    pub frames_sent: u64,
    pub frames_gated: u64,           // no room in send buffer or capture
    pub frames_refused: u64,         // refused by the rate limiter
    pub pacing_rate: Option<u64>,    // transport estimate, bit/s, diagnostic
    pub estimate: Option<u32>,       // host controller estimate, bit/s
    pub padding_sent: u64,           // padding datagrams
    pub pipeline_micros: u32,        // median capture-to-wire latency
}
```

`HostHello` is the first message the host writes. It carries nothing about
encoding: the host decides that, and states it in `StreamConfig` after it reads
the client's hello. `audio` is in the hello because the client must know it
before it draws its controls. The host probes it on its own machine, as it
probes codecs: a sound server that answers, and an Opus encoder element that is
installed. The client shows the audio control only when `audio` is true. A
control that does nothing looks like a fault.

After the hellos, the host sends `Stream` first, then `Audio` if `audio` is
true.

`Stream` carries the host's encoding decision. The host sends it again when any
field changes, including a change that congestion forces. Every field except
`codec` can change during a session. It carries no codec configuration bytes:
the decoder reads geometry from the parameter sets in the stream (see
[Constraints the client places on the encoder](#constraints-the-client-places-on-the-encoder)).
`display_aspect` is here because the bitstream cannot carry it: the SPS of a
16:10 capture scaled to 16:9 declares no aspect ratio. `framerate` is not a
hint: the encoder budgets bits per frame as `bitrate / framerate`.

`Heartbeat` goes out every 250 ms (`HEARTBEAT_INTERVAL`), whether or not frames
flow. The client reads liveness from control messages, not from video. Capture
blocks while the screen is static, with idle gaps measured up to 1.53 s, so
silence on the video path does not separate an idle host from a dead one. QUIC
keep-alive does not answer the question: the application never sees it.

| Client state | Condition |
|---|---|
| Streaming | A picture arrived in the last 200 ms. |
| Idle | No recent picture, and a host message arrived in the last 4 s. |
| Lost | No host message for 4 s (16 heartbeat intervals). |

Any host message is proof of life, not only a heartbeat. The limit is wide
because a heartbeat is a stream frame, and QUIC puts video datagrams ahead of
stream frames in a packet. A saturated link delays heartbeats long before it
drops the connection: on Wi-Fi, a limit of three intervals fired while video
still arrived. 4 s stays below the 5 s idle timeout of the transport.

The interval is a constant, not a field. The client reads the *absence* of a
heartbeat, so it must know the interval before the first one arrives.

### The cursor

Capture asks the compositor to leave the cursor out of the frame, and reads the
pointer's position, hotspot and picture from a separate cursor session.
`CursorUpdate` is the only thing that puts the pointer back. This is deliberate:
a pointer inside the picture lags by the whole pipeline (capture, encode, send,
decode, draw). The client draws the pointer itself, moves it with the finger,
and takes the updates as corrections.

```rust
pub enum CursorUpdate {
    Tracked(CursorReport),
    Untracked, // no cursor session; sent once, nothing follows
}

pub struct CursorReport {
    pub screen: Size,                  // captured output, logical pixels
    pub at: Option<CursorPoint>,       // None: not over the captured output
    pub image: Option<CursorPicture>,  // None: keep the last picture
}

pub struct CursorPoint {
    pub x: i32,         // hotspot from the left edge; may fall outside
    pub y: i32,         // hotspot from the top edge
    pub hotspot_x: i32, // hotspot inside the picture, from its left edge
    pub hotspot_y: i32, // the same, from its top edge
}

pub enum CursorPicture {
    Whole(CursorImage),    // a picture the client has not been sent
    Known(CursorImageId),  // a picture the client holds, by name
}

pub struct CursorImageId(pub u32);

pub struct CursorImage {
    pub id: CursorImageId,
    pub width: NonZeroU16,  // rows, in the picture's own pixels
    pub height: NonZeroU16,
    pub scale: NonZeroU8,   // picture pixels per logical pixel
    pixels: Vec<u8>,        // width * height * 4: premultiplied B, G, R, A
}
```

**Units.** Every position is in logical pixels of the captured output, the units
the compositor measures the pointer in. On a scaled output these are not the
captured pixels, and logical pixels are the one space in which every figure is
exact. A picture keeps its own finer rows and covers `width / scale` by
`height / scale` logical pixels. The client divides each figure by `screen` and
multiplies by the box it draws the video into. The axes scale independently, so
a capture whose aspect ratio the encoder corrected also comes out right.
`screen` is not the encoded size, which the resolution ladder changes at any
moment. It travels in every report, so each report is self-contained.

**Pictures travel once.** A 48-pixel square picture is 9216 bytes, about seven
hundred times a bare position, and it shares the reliable stream with input.
Resent with each step, it would queue in front of the next click. So a report
carries a picture only when it changes, and a picture the client already holds
travels as `Known(id)`. An identifier names a shape: a pointer that turns into a
text beam and back gets its earlier identifier again. Both ends keep the last 32
pictures sent (`CursorImage::REMEMBERED`) and drop the oldest first. The host
sends `Known` only for a picture inside that window. The lists match because
both come from one ordered stream.

**Size cap.** A picture is at most 57,600 bytes (`CursorImage::MAX_BYTES`,
120 x 120 at four bytes a pixel). The host does not send a larger picture. It
still sends the position, and the client draws a pointer of its own.

**Validation.** `pixels` is private. Its accessor returns `None` unless the
length is exactly `width * height * 4` and within `MAX_BYTES`. The encoding
cannot stop a peer that lies about its rows, and the reader is a rasteriser.

**Timing.** The host checks the pointer every 16 ms and sends a report only when
something changed, so a still pointer costs nothing. `at: None` means the
pointer is on another monitor: the client draws nothing, and does not pin the
pointer to an edge.

#### What the client does with it

The client keeps its own position as a fraction of the screen and moves it the
instant the finger moves. A host report only corrects that position. It
describes the host as it was about half a round trip ago, so the client replays
its own later movement (up to 500 ms of it) on top of the report before it
compares the two. Without the replay, a steady drag rubber-bands once per
report.

- A disagreement under 5% of the screen is eased away at 35% per report. That
  clears it in about a tenth of a second, and one wrong report cannot jump the
  pointer.
- A larger disagreement is not drift: something else moved the host's pointer,
  for example the host's own mouse. The client takes the host's position at
  once.
- The client measures the ratio of the host's reported motion to its own (the
  host's pointer acceleration) and applies it to its prediction.

On `Untracked`, the client draws its own arrow at the position it has steered.
Nothing corrects it, so it drifts, and the client marks it as its own pointer,
not the host's. A trackpad with a drifting pointer is usable. A trackpad with no
pointer is not.

## Sound

The host streams what its own speakers would play. The capture tap is the
monitor of the default sink, which is everything the machine plays. There is no
separate mixer and no per-application selection.

```rust
pub enum AudioMode {
    Off,       // nothing captured or streamed, host output untouched
    Exclusive, // streamed, and the host's own sink muted
    Shared,    // streamed, and the host plays it too
}

pub enum AudioCommand {
    SetMode(AudioMode),
    SetVolume(u16), // permille of the sink's 100% (VOLUME_NORM = 1000)
    SetMuted(bool),
}

pub struct AudioStatus {
    pub mode: AudioMode, // what the host does, not what it was asked
    pub volume: u16,     // sink volume, permille
    pub muted: bool,     // the sink itself is muted
}
```

`Exclusive` streams to the client and mutes the host's sink, so the sound
follows a listener to another room. The tap is on the sink's monitor ports,
ahead of the sink's volume and mute. So muting the sink silences the host and
changes no byte of the stream.

`Off` is where a session starts and where it must end. The host restores its
mute when the mode leaves `Exclusive`, when the client disconnects, and when the
daemon stops. A person at the host afterwards must not find the machine silent.

`AudioStatus` reports what the host **is** doing. A host that cannot start
capture, or refuses a mode, reports the mode it is really in. The client's
control follows the report, not its own last click.

The host sends `AudioStatus` after `Stream` at session start, in answer to every
`AudioCommand`, and whenever the sink changes under the session, including a
change at the host's own keyboard. Without that, the two ends disagree about the
volume until one of them is touched again.

### Volume

The host sink's volume is the only volume. The client has no gain control of its
own. This follows from the tap position: the monitor ports carry the same
samples at any sink volume, so a client knob would change what the client hears
while the host's control showed something else.

The host applies `SetVolume` to its sink and reports the result. The client
turns the reported value into playback gain (`AudioStatus::gain`):

```
gain = (min(volume, 1000) / 1000)^3
```

Permille is a position on a control, not a ratio of amplitude. PulseAudio, and
the pipewire-pulse server that stands in for it, cube the control to get
amplitude, so the midpoint of the control is an eighth of full amplitude. A
client that applied the number directly would be louder than the host at every
setting except the two ends.

One knob then works in both streaming modes. In `Shared` it moves the host's
speakers and the client's playback together. In `Exclusive` the sink is muted,
so it moves only what the client hears, and the number is still the host's.

The end that reads a volume above 1000 clamps it. The encoding cannot forbid
such a value, and the reader cubes it: unclamped, a peer that sends 10,000 puts
a thousandfold gain into a listener's ears.

`muted` is the host's own silence and nothing else. It reads true for all of
`Exclusive`, while the client hears everything. A client that applied it would
hear nothing in the one mode chosen to make it hear everything.

## Input events

| `InputEvent` | Fields | Units and rules |
|---|---|---|
| `PointerMotion` | `dx`, `dy: f32` | Relative motion, in pixels of the streamed image. The host scales it by the ratio of its capture to the size it encodes, so the pointer keeps its feel when the ladder steps. |
| `PointerMotionAbsolute` | `x`, `y: f32` | Position as a fraction of the streamed image, `0.0` to `1.0` from the left and top edges. For a desktop client whose mouse hovers over the video. |
| `PointerButton` | `button: PointerButton`, `press: Press` | `Left`, `Right`, `Middle`, `Back` or `Forward`. `Down` or `Up`. |
| `Wheel` | `dx`, `dy: f32` | Scroll pixels of the client's own screen, not of the image. A detent is a notch, not a distance, so scaling it by the picture would change what one notch does at each ladder step. |
| `FingerScroll` | `dx`, `dy: f32` | Finger scroll, in pixels of the streamed image, so the content keeps up with the finger at every ladder step. |
| `FingerScrollEnd` | | The fingers left the surface. Applications need it to start and stop kinetic scrolling. |
| `Key` | `usage: HidUsage`, `press: Press` | A USB HID usage (`page: u16`, `id: u16`) changed state. The host injects only from the keyboard page (`0x07`) and checks the page first: Android reports a page outside HID when an input method intercepts a key. The client sends only real transitions, because the host repeats a held key itself. |
| `Text` | `String` | UTF-8 text from an input method, such as a phone's on-screen keyboard. Insertion only. |

Scroll sign: positive `dy` scrolls down (the content moves up), and positive
`dx` scrolls right. This matches `wl_pointer.axis` and the wheel delta of every
toolkit, so the host injects the numbers unchanged. A touch surface where the
content follows the finger sends the opposite of the finger's motion. The client
does that inversion.

Keys travel as HID usages, not characters. The host owns the keyboard layout, so
a client with a different layout still produces the key that was pressed.

Text travels separately, because an on-screen keyboard produces characters and
most characters have no HID usage. The host builds a keymap that puts each
needed character on a key, and injects that key. Delete and Enter have real HID
usages, so a client sends them as `Key`.

### Touch gestures

The phone is a trackpad, so pointer motion is relative by default. The pointer
does not follow the finger. The finger pushes it, as on a laptop trackpad. So
the whole screen is reachable, the hand does not cover it, and the pointer does
not jump to the touch point.

| Gesture | Effect |
|---|---|
| One-finger tap | Left click |
| Two-finger tap | Right click |
| Three-finger tap | Show or hide the on-screen keyboard |
| One-finger drag | Move the pointer |
| Two-finger drag | Scroll |
| Long press, then drag | Move the pointer with the left button held |
| Three-finger swipe up | Hide the client's own controls. Sends nothing to the host. |

Gestures differ by finger count, never by tap count, so each gesture resolves
the moment the fingers lift. A tap already sent cannot be taken back. To tell a
single tap from a double tap, the client must either wait to see if a second tap
follows (Flutter's gesture arena held a single tap for a measured 310 ms) or
send a real left click before every right click. Neither fits a latency budget
of 10 to 20 ms.

## Constraints the client places on the encoder

Coded width and height are multiples of 16 (`CodedSize::ALIGNMENT`). H.264
codes a height of 1080 as 1088 plus a crop rectangle, and Flutter's Android
presentation path does not apply the crop. Every rung of the resolution ladder
is a multiple of 16, which avoids the problem instead of detecting it.
`CodedSize` cannot hold an unaligned size, so none reaches the wire.

The encoder repeats the parameter sets (H.264 SPS and PPS, the AV1 sequence
header) before every key frame, not only at session start. So the client needs
no separate decoder configuration step, survives a reconnection or a resolution
change without one, and recovers by itself if a parameter set is lost on a path
that never retransmits. This is measured for both codecs: 4 key frames in a
200-picture AV1 run carried 4 sequence headers.

The stream reorders no frames, and the sequence parameter set declares it. H.264
runs with `b-frames=0`, and AV1 runs with no forward references. A decoder that
is not told this holds frames back, which adds latency that no client setting
can remove.

## Datagram path

Every datagram starts with a fixed 16-byte header (`HEADER_LEN`), written by
hand, byte by byte. This path carries thousands of packets a second and is the
part of the protocol most worth auditing, so the layout is specified here as
bytes, not derived from a Rust type. All fields are little-endian. There is no
RTP framing and no per-packet authentication tag: QUIC supplies encryption and
connection identity.

### Datagram kinds

A datagram is one of four kinds:

1. Bit 3 of `flags` set: audio.
2. Bit 2 of `flags` set: padding.
3. Otherwise, `slice_id` 255: parity.
4. Otherwise: a picture packet, which is a fragment of one slice.

All three markers are at fixed offsets, so classification is three comparisons
and no parsing. The receiver reads bits 2 and 3 before any field that describes
a frame, because neither audio nor padding belongs to a frame and their fragment
fields describe nothing. Both bits set is malformed, not resolved by precedence:
one says the payload is sound, the other says it carries nothing, and a guess
would play whichever the sender did not mean.

Audio takes a flag bit, not a `slice_id` value, for that reason. `slice_id` is
read after the fragment fields, so marking audio the way parity is marked would
mean validating, on every audio packet, a fragment count and index the sender
never wrote. Also, `slice_id` 254 would cost the picture a slice it can use.

### Header layout

Fields common to all kinds:

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 8 | 2 | `seq` | Session-wide datagram counter, so a gap shows across a frame boundary. Wraps, and is compared circularly as in RFC 1982. Every kind is counted: each costs the path the same bytes, and its loss is the same evidence about the path. |
| 15 | 1 | `flags` | Bit 0: key frame. Bit 1: last slice of the frame. Bit 2: padding. Bit 3: audio. Bits 4 to 7 are spare, and a receiver must ignore them. Bits 0 and 1 have meaning only on a picture packet. |

Fields common to picture and parity:

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 4 | `frame_id` | Reassembly key, and what a recovery request names. Counts encoded pictures and skips none. |
| 14 | 1 | `slice_id` | Slice index, 0 to 254. 255 marks parity. |

Picture packet, `slice_id` below 255:

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 4 | 4 | `capture` | Capture time, in 100-microsecond ticks since the session began. |
| 10 | 2 | `slice_pkts` | Packets in this slice. Every packet carries it, so any one of them can size the reassembly buffer. |
| 12 | 2 | `pkt_idx` | This packet's position in the slice. |

Parity packet, `slice_id` 255:

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 4 | 2 | `data_shards` | Picture datagrams in the frame. A decoder needs this many shards, of either kind, to rebuild it. |
| 6 | 2 | `tail` | Video bytes in the frame's last picture datagram. |
| 10 | 2 | `parity_shards` | Parity packets the frame carries. |
| 12 | 2 | `parity_idx` | This packet's position among them. |

The payload follows the header and is at least one byte. At the 1434-byte
datagram this transport reaches on a 1500-byte path, 1418 bytes of video remain,
which is 94.5% of the wire frame. The host rounds the payload width down to an
even number, because the Reed-Solomon coder works on two bytes at a time.

`capture` counts 100-microsecond ticks because microseconds overflow four bytes
after 71 minutes, and sessions run longer. At 100 microseconds the same four
bytes last 119 hours. The counter wraps, and differences are taken circularly.
Nothing reads it at a finer resolution: latency is reported in milliseconds,
and the receiver report uses 250-microsecond ticks.

Receivers compare `frame_id` directly, not circularly. Four bytes last over a
year at 120 frames a second.

No field counts the slices in a frame. The last-slice flag supplies the count,
so the host can send slice 0 while the encoder still produces slice 1. Slices
are independently decodable units from the encoder: a lost slice costs a band of
the picture, not the whole frame. Fragmentation within a slice is a separate
axis, carried by `slice_pkts` and `pkt_idx`. Fragmentation alone does not give
that property, because a lost fragment still destroys its slice.

### Validation

A receiver rejects a datagram that:

- is shorter than 16 bytes, or has no payload;
- has both bit 2 and bit 3 set;
- is picture or parity and has `slice_pkts` 0, or `pkt_idx` not below
  `slice_pkts`;
- is parity and has an odd payload length, `data_shards` 0, `tail` 0, or `tail`
  longer than the payload.

On audio and padding the receiver does not read the fragment fields.

### Forward error correction

Parity is Reed-Solomon over the frame's picture datagrams, in send order. Shard
`i` is the payload of datagram `i`, padded with zeroes to the payload width. The
coder produces `parity_shards` more shards of the same width. Any `data_shards`
of the `data_shards + parity_shards` shards rebuild the frame. A parity payload
is one shard, as wide as a full picture payload, so the datagram's length gives
the shard width and no field carries it. `tail` gives the length of the last
picture payload, so a rebuilt frame has its exact length.

The parity fields fit the same 16 bytes. A repair has no capture time, so it
uses those four bytes for `data_shards` and `tail`. `parity_shards` and
`parity_idx` sit where `slice_pkts` and `pkt_idx` sit and obey the same rule
(index below count), so one check serves both. That reuse gives two rules:

- **A parity packet never opens a frame.** A frame of which no picture datagram
  arrived needs more parity than picture to rebuild, and no parity level reaches
  that (the ceiling is 50%). So the capture time and the key-frame flag are
  always the picture's to state.
- **Only a frame with exactly one slice carries parity.** Parity indexes the
  frame's datagrams in send order. With one slice, `pkt_idx` gives that index.
  With two, a receiver would need each slice's fragment count to place a rebuilt
  shard, and there is no room to state it. The encoder runs with one H.264 slice
  or one AV1 tile, and [`findings.md`](findings.md) recommends firmly against
  more. A multi-slice frame goes out without parity.

The host sends a frame's parity after its picture. A frame that arrives whole is
complete on its last picture datagram, so its parity arrives after the frame is
decided. The receiver counts that parity and discards it. It must not count it
as late, or a perfect link reports a fifth of its datagrams late.

The receiver attempts recovery only for a frame known to be damaged, when its
wait ends or a later frame completes. A stream that loses nothing never runs the
decoder. A damaged frame waits one frame period, from `StreamConfig::framerate`,
for a reordered packet or its repair. The wait is never below 15 ms
(`REORDER_WINDOW`).

#### Parity level

The level is the host's choice and is not announced. Each parity packet
describes its own frame's block, which is all a receiver reads. The host applies
this rule (`tachyx_proto::packet::Parity`):

- A frame carries `ceil(data_shards * percentage / 100)` parity shards.
- A key frame gets the level plus half, capped at 50%.
- The session starts at 20% (`Parity::MEASURED`), the level of the measurement
  in [`findings.md`](findings.md).
- Each receiver report gives a wanted level, linear in the reported loss: about
  7.2 times the loss percentage, so 2.79% loss asks for 20%. The wanted level is
  never below 5% (`FLOOR`) or above 50% (`CEILING`).
- The level rises on the first report that asks for more. It falls only after 4
  consecutive reports ask for at least 3 points less.
- After 20 consecutive reports with no loss (one second), the wanted level is 0,
  and parity stops once the fall rule allows it. The first report with any loss
  restores the floor.

At the 2.79% loss measured in [`findings.md`](findings.md), 20% parity delivered
99.9% of frames for **less** total bandwidth than the key-frame-on-demand
recovery it replaces: a recovery key frame costs about eight frames, and parity
costs a fifth of one. A key frame gets more parity because a lost key frame
costs the request and the answer, and the answer is the frame most likely to be
lost next. The level rises fast and falls slowly: too little parity costs a
broken reference chain and a key frame, and too much costs a few percent of the
link.

### The frame identifier counts encoded pictures, and skips none

This is a requirement on the host. The receiver depends on it.

A frame that loses some packets shows it: `seq` has a gap between two of its
packets, or the frame is still open when the next frame's packets arrive. A
frame that loses *every* packet leaves no trace except the hole in the frame
identifiers. So the receiver reads that hole. A delivered frame whose identifier
is not one above the last delivered one means the frames between are gone, the
reference chain is broken, and nothing may be decoded until the next key frame.

That reading is correct only if the host never skips an identifier for a reason
of its own. So the identifier follows the encoder's chain of reference frames,
not the capture clock:

- A frame the host declines **before** encoding costs no identifier. The encoder
  never saw the picture, its reference chain is unbroken, and the client has
  nothing to recover. The send gate asks this cheap question.
- A frame the host declines **after** encoding (the send gate's second check, or
  any encoded picture thrown away) keeps its identifier and leaves a gap. The
  encoder's reference chain has already advanced past it, so every later frame
  refers to a picture the client will never hold. The gap is the signal that a
  key frame is necessary. It looks the same as wire loss because the effect is
  the same.

Numbering by capture would make the two cases look the same on the wire. With
one reference frame and a key frame interval of 1024 at 60 Hz, that is up to
17 seconds of silently wrong picture. No extra field says which case occurred:
the identifier already says it.

Identifiers do not restart within a connection. The resolution ladder is a caps
filter in front of the encoder and does not restart it.

### Padding

A datagram with bit 2 set has the 16-byte header and a payload of zeroes, at
least one byte long. It lets the host measure a rate that the picture does not
supply. A still desktop produces no frames, and a congestion controller that
learns only from media never learns that the link has recovered.

The host writes `frame_id` 0, `slice_pkts` 1 and `pkt_idx` 0, and leaves the
other picture fields zero. A receiver counts its `seq`, records its arrival for
the next `Feedback`, and discards it. No other field has meaning, and the
datagram must not reach reassembly.

### Audio

A datagram with bit 3 set carries exactly one Opus packet.

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 4 | `frame_id` | Counts the session's audio packets and skips none, for the same reason as the picture identifier: a packet lost whole leaves only the hole to say it existed. A receiver reads the hole as sound to conceal. |
| 4 | 4 | `capture` | When the samples were captured, on the picture's clock and in the same 100-microsecond ticks, so the two line up without a mapping between timelines. |
| 10 | 5 | | Zero on write, ignored on read. |

Bit 0 of `flags` is clear. There is no length field, no channel count and no
sample rate. An Opus packet states its own duration and channel layout in its
table of contents, and the datagram's length is the packet's length.

The five zero bytes are the opposite of what padding writes there, on purpose.
`slice_pkts` 0 is not a valid slice, so a receiver that does not know the audio
bit rejects the datagram by name. It does not hand a video decoder an Opus
packet as a band of picture.

Audio takes part in sequence accounting and nothing else: one Opus packet is one
datagram, so there is nothing to reassemble. A report that skipped it would
leave about a tenth of the session's datagrams out of every delay measurement.

The host does not pace audio with the picture and must not queue it behind a
frame. A key frame is released over tens of milliseconds, so audio behind it
arrives that late at every key frame. A gap in the picture is one frame nobody
sees. A gap in sound is a click everybody hears.
