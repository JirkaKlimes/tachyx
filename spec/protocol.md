# tachyx wire protocol — draft 2

Status: draft. Covers only what the current milestone implements: one client, one
host, one LAN. Pairing, authentication and relaying are specified elsewhere when
they exist.

Draft 1 defines `slice_id` 255, which draft 0 reserved. A draft 0 client reads a
parity packet as a slice of the picture and then completes no frame at all, so
the version had to move rather than the field being added quietly.

Draft 2 adds the set of codecs a client can decode to `ClientHello`. `postcard`
writes no field names, so a draft 1 hello read as a draft 2 one ends a field
short and a draft 2 hello read as a draft 1 one leaves a byte over. Neither
fails to parse on its own, which is what the version exists to catch.

## Transport

A single QUIC connection carries everything.

| Path | QUIC object | Reliability |
|---|---|---|
| Video | DATAGRAM (RFC 9221) | unreliable, never retransmitted |
| Control and input | one bidirectional stream, opened by the client | reliable, ordered |

Video never shares a stream with control. A lost video packet must not delay an
input event, and an input event must not delay a frame.

### Security, currently

The host presents a self-signed certificate derived from a key it keeps, and the
client pins the SHA-256 of that certificate's subject public key info the first
time it sees a host under a given name. Afterwards a different key under the same
name is refused. The pin is over the public key rather than the certificate
because the host derives a fresh certificate from the stored key on every start,
and a certificate is not byte-stable across those starts.

Validity dates and the subject name are deliberately not checked. The certificate
is self-signed, so a forger writes whatever it likes into both; the pin is the
only thing an attacker has to break.

What is missing is the first meeting. A host the client has not met is accepted
and remembered, so an attacker present at that moment is pinned instead of the
host. Pairing closes that hole, and it is one verifier away.

## Control stream framing

Each message is a `u32` big-endian byte length followed by that many bytes of a
`postcard`-encoded message.

The message types are defined once, as ordinary Rust types that make invalid
states unrepresentable, and serialised with serde. Every consumer of this control
plane is Rust, so a schema compiler would buy nothing but a second set of weaker
types and the conversion layer between them.

`postcard` varint-encodes integers and lengths, needs no allocator, and has a
published wire specification. It is not self-describing, so the first message in
each direction carries the protocol version as its first field, and each end
reads that field before it decodes anything after it. A mismatch is then reported
by name rather than misparsed.

Both directions, because a check in one direction only leaves the other end
decoding a message a later version reshaped, and those bytes do not fail to
parse. They parse into something else.

The host writes its hello before it reads the client's, so the order the two
arrive in never matters. A refusal closes the connection with the reason
attached, and that is what a refused client actually reads: closing a QUIC
connection discards stream data the peer has not acknowledged, so the hello
loses the race to the close. Measured on loopback, a client claiming version 7
sees `closed by peer: peer speaks protocol version 7, this build speaks 0 (code
1)`, which is a named answer rather than a reset.

The video path does not use this encoding, or serde at all. See below.

## Control messages

### Client to host

`ClientHello` — first message on the stream.
: Decoders the client can drive (codec, profile, maximum level, resolution and
  frame rate, bit depth, chroma subsampling), the viewport in physical pixels,
  the device pixel ratio, and the requested resolution mode.

  The codec part is a set, and at draft 2 it is the only part of that list that
  is really there. Each end probes the machine it runs on rather than assuming
  its platform: the client asks which decoder elements are installed, the host
  asks which encoder elements are, and the host encodes the best codec the two
  sets share. The preference is AV1 then H.264, written down once in the
  protocol so that both ends can work out the same answer. H.264 is in both
  sets on every platform either end ships for, so the fallback is not a
  hypothetical branch. No codec in common ends the session: there is no picture
  to send.

  The set travels as a bit set rather than a list of codec names. `postcard`
  refuses an enum discriminant it does not know, so a list carrying a codec
  added in a later version would fail to decode and take the whole hello with
  it; a bit nobody knows is simply never in an intersection.

  The host reads the set at the hello and nowhere else. A client builds its
  decoder once and cannot rebuild it without losing every reference picture it
  holds, so the same set travels in every later `ViewportChanged` and means
  nothing new there.

`ViewportChanged`
: The same capability payload, sent again. The host treats it as renegotiation.
  Sent on rotation, window resize, or when the user changes the resolution mode.

`InputBatch`
: A batch of input events, each with the client's monotonic timestamp. Batched on
  a short tick rather than sent individually, so a burst of pointer motion costs
  one packet.

`Feedback`
: The receiver report that drives congestion control. When each video datagram
  arrived, which never did, and cumulative counts of both. Specified below and
  in [`congestion.md`](congestion.md).

`KeyframeRequest`
: Sent when reassembly failed and the client cannot continue from what it has.

`ClockSyncRequest`
: Carries the client's send timestamp. Used to put the two clocks on a common
  basis so per-stage latency can be attributed. Congestion control does not need
  it: every quantity the estimator reads from a `Feedback` is a difference
  between two of the client's own arrival times, so a constant offset between
  the clocks cancels exactly.

### The receiver report

```rust
pub struct Feedback {
    /// The `seq` of the first datagram this report accounts for.
    pub base: Seq,
    /// The client's own clock at the arrival of the first datagram in
    /// `packets` that arrived, in 250-microsecond ticks since the client's
    /// session began.
    pub reference: u32,
    /// One entry per sequence number from `base` upward, in order, no gaps.
    pub packets: Vec<PacketArrival>,
    /// Datagrams the client has never seen, since the session began.
    pub lost: u64,
    /// Datagrams the client has received, since the session began.
    pub received: u64,
}

pub enum PacketArrival {
    /// Not received when the report was built.
    Missing,
    /// Received this many 250-microsecond ticks after the previous datagram in
    /// this report that arrived. Negative when the two were reordered. The
    /// first arrival is measured from `reference`, so its delta is zero.
    Received(i16),
}
```

The key is the `seq` already in the media header, so nothing new identifies a
packet. Deltas rather than offsets because `postcard` varint-encodes and a delta
between consecutive datagrams is almost always one byte. Signed because
reordering is real, and 250-microsecond ticks because a signed sixteen-bit tick
count then spans 8.19 seconds, thirty times the longest report interval.

The counters are cumulative rather than per-interval so that a delayed report is
still exactly right about the interval it covers.

A report goes out every 50 ms while datagrams are arriving, carries at most 512
statuses, and drops to every 250 ms while the host's announced bitrate is under
250 kbit/s. Measured with real encodings, a report describing 88 datagrams is
192 bytes, which at twenty a second is 0.17% of the 20 Mbit/s it describes.

### Host to client

`HostHello` — first message on the stream, sent before the client's is read.
: The protocol version, and at draft 0 nothing else. There is nothing else for
  the host to announce at this point: the host decides how to encode, not the
  client, and it says so in `StreamConfig` once it has read what the client can
  drive. Encoders, capture formats and the list of monitors join it when
  choosing a monitor is a thing a client can do.

`StreamConfig`
: The host's decision: codec, coded size, visible size, display aspect ratio,
  frame rate and target bitrate. Re-sent whenever any of those change, including
  a change forced by congestion. Everything but the codec may change while a
  session runs; the codec is settled at the hello and never moved.

  It carries no codec configuration bytes: the parameter sets are repeated in
  the bitstream before every key frame, so the decoder takes the new geometry
  from the stream itself and a resolution change needs no separate
  configuration step. That is measured rather than assumed for both codecs, and
  the AV1 sequence header behaves as the H.264 parameter sets do: 4 key frames
  in a 200 picture run carried 4 sequence headers. The display aspect is here
  because it is the one thing the bitstream cannot say: the sequence parameter
  set of a 16:10 capture scaled to 16:9 declares no aspect ratio at all.

`Heartbeat`
: Proof the host is alive, and what it has been doing: its session clock, the
  last frame it sent, frames sent and gated, the transport's bandwidth estimate,
  and its median capture-to-wire latency. Sent four times a second whether or
  not frames are flowing.

  The host must say this out loud, and the client must read liveness from it
  rather than from the video. Capture blocks while the screen is static, with
  idle gaps measured up to 1.53 seconds, so silence on the video path does not
  distinguish an idle host from a dead one. A client that treated no frames as a
  fault would report one every time the user stopped to read something, so it
  shows an idle screen as idle and reports a lost connection only after three
  missed heartbeats. QUIC's own keep-alive does not answer the question either:
  it is a transport ping the application never observes.

  The interval is fixed rather than announced, because the client needs it
  before it has received one: what it reads is the *absence* of a heartbeat.

`CursorUpdate`
: Where the pointer is and what it looks like. Specified below.

`ClockSyncReply`
: The client's timestamp, the host's receive timestamp, and the host's send
  timestamp. Four timestamps in total give offset and round trip.

`HostStats`
: Per-stage timings measured on the host, for the client's statistics overlay.

### The cursor

Capture asks the compositor to leave the cursor out of the frame, and asks a
separate cursor session for its position, its hotspot and its picture. So the
pointer is not in the video, and this message is the only thing that puts it
back.

That is deliberate rather than a limitation. A pointer that arrived inside the
picture would move only as fast as a frame could be captured, encoded, sent,
decoded and drawn, so every flick of the finger would drag a pointer that lags
by the whole pipeline. Drawn locally it moves with the finger, and these updates
correct it.

```rust
pub enum CursorUpdate {
    /// The compositor reports the pointer.
    Tracked(CursorReport),
    /// The compositor offers no cursor session. Sent once; nothing follows it.
    Untracked,
}

pub struct CursorReport {
    /// The captured screen, which every other figure here is measured in.
    pub screen: Size,
    /// Where the pointer is, or `None` while it is not over that screen.
    pub at: Option<CursorPoint>,
    /// The picture, in the update that changes it and in no other.
    pub image: Option<CursorImage>,
}

pub struct CursorPoint {
    /// The hotspot, from the left edge of the captured screen, in its pixels.
    pub x: i32,
    /// The same, from the top edge.
    pub y: i32,
    /// Where the hotspot sits inside the picture, from its left edge.
    pub hotspot_x: i32,
    /// The same, from its top edge.
    pub hotspot_y: i32,
}

pub struct CursorImage {
    pub width: NonZeroU16,
    pub height: NonZeroU16,
    /// `width * height` pixels, row by row, four bytes each: blue, green, red,
    /// then alpha, with the colour already multiplied by the alpha.
    pixels: Vec<u8>,
}
```

Every figure is in pixels of the **captured screen**, not of the encoded
picture. The resolution ladder changes the second at any moment, and it would
otherwise change the meaning of every number here in the middle of a session.
The client divides by `screen` and multiplies by the box it draws the video
into, which is also what makes a capture whose aspect ratio the encoder
corrected come out right: the two axes are scaled independently.

`screen` travels with every report rather than once. It is four bytes and it
makes the message answerable on its own, which the alternative -- deriving it
from `StreamConfig` -- would not, because `StreamConfig` describes the encoded
picture and this does not.

**The picture travels only when it changes.** A 48-pixel cursor is 9216 bytes,
about seven hundred times a bare position, and this is the same reliable stream
input travels on: resending it with every step of the pointer would put the
pointer's own picture in front of the user's next click. A picture larger than
`120 x 120` does not fit in one control message and is not sent; the position
still is, and the client draws a pointer of its own.

`pixels` is private, and read through an accessor that returns `None` unless the
length is exactly `width * height * 4`. Nothing in the encoding rules out a peer
that lies about its own rows, and what would read them is a rasteriser.

The host looks at the pointer every 16 ms and sends only what changed, so a
still pointer costs nothing. `at` of `None` means the pointer is on another
monitor: the client draws nothing rather than pinning it to an edge.

#### What the client does with it

The client keeps its own position, as a fraction of the screen, and moves it the
instant the user's finger does. A host report is a correction, and correcting is
not the same as obeying: the report describes the host as it was about half a
round trip ago, so everything the client has done since is replayed on top of it
before the two are compared. Without that replay a steady drag rubber-bands, one
report per frame.

A disagreement under 5% of the screen is eased away at 35% per report, which
clears it inside a tenth of a second and cannot jump the pointer on one wrong
report. A disagreement above that is not drift -- somebody moved the host's own
mouse -- and is taken outright.

`Untracked` is answered by drawing the client's own arrow at the position it has
been steering. Nothing corrects it, so it drifts, and it is drawn as the
client's own mark rather than as the host's cursor. A trackpad with a pointer
that drifts is usable; one with no pointer at all is what this exists to fix.

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

A datagram carries either a piece of the picture or a piece of the parity that
repairs it, and `slice_id` says which. 255 means parity; anything else is a
slice of the picture. That byte is read at a fixed offset before anything that
depends on it, so telling the two apart costs one comparison.

Four fields mean the same thing in both:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | `frame_id` | Reassembly key, and what a recovery request names. Counts encoded pictures and skips none. |
| 8 | 2 | `seq` | Session-wide datagram counter, so a gap is visible across a frame boundary. Compared circularly, in the manner of RFC 1982. Parity packets are counted in it like any other: they cost the path the same bytes and their loss is the same evidence about it. |
| 14 | 1 | `slice_id` | Which slice of the frame, or 255 for parity. |
| 15 | 1 | `flags` | Bit 0 marks a key frame, bit 1 the frame's last slice, bit 2 padding. The rest are spare and a receiver must ignore them. Bit 1 is meaningless on a parity packet. |

A picture packet, `slice_id` under 255:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 4 | 4 | `capture` | When the frame was captured, in units of 100 microseconds since the session began. |
| 10 | 2 | `slice_pkts` | Packets in this slice. Every packet carries it, so any one of them can size the reassembly buffer. |
| 12 | 2 | `pkt_idx` | This packet's position in the slice. |

A parity packet, `slice_id` 255:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 4 | 2 | `data_shards` | Picture datagrams the frame was cut into, which is how many shards of either kind a decoder needs to rebuild it. |
| 6 | 2 | `tail` | Video bytes in the frame's final picture datagram. |
| 10 | 2 | `parity_shards` | Parity packets the frame carries. |
| 12 | 2 | `parity_idx` | This packet's position among them. |

The payload follows immediately. At the 1434-byte datagram this transport reaches
on a 1500-byte path, that leaves 1418 bytes of video, or 94.5% of the wire frame.
A parity payload is one shard and is exactly as wide as a full picture payload,
so the shard length needs no field of its own: the datagram's own length is the
answer, and the packetiser rounds the payload down to an even number of bytes
because the coder works two bytes at a time.

### Forward error correction

Parity is Reed-Solomon over the frame's own picture datagrams, in send order.
Shard `i` is datagram `i` padded with zeroes to the payload width, and the coder
produces `parity_shards` more of the same width. Any `data_shards` of the
`data_shards + parity_shards` rebuild the frame.

Three fields is everything a receiver needs and they fit the sixteen bytes that
were already there, because a repair has no capture time of its own and the four
bytes a picture spends on one are free. `parity_shards` and `parity_idx` sit
where `slice_pkts` and `pkt_idx` sit, and mean the same thing about a different
group, so the check that an index falls inside its count is one piece of code
for both.

Two rules follow from that reuse and both are deliberate.

**A parity packet never opens a frame.** A frame of which no picture datagram
arrived would need more parity than picture to rebuild, which no parity level
this system sets ever reaches, so such a frame is lost either way. Keeping the
rule means the capture time and the key-frame flag are always the picture's to
state and never have to travel on a repair.

**A frame that carries parity has exactly one slice.** Parity indexes the
frame's datagrams in send order, and only one slice makes that index recoverable
from `pkt_idx` alone; with two, a receiver would have to know each slice's own
fragment count to place a rebuilt shard, and there is no room to say so. The
encoder is configured for one slice and [`findings.md`](findings.md) recommends
firmly against more, so a multi-slice frame simply goes out unprotected.

Parity is generated after the picture and sent behind it, which has one visible
consequence: a frame that arrives whole is delivered by its last picture
datagram, and its parity lands after the frame has already been decided. A
receiver counts that parity and discards it, and must not count it as a late
packet, or a perfect link reports a fifth of its datagrams late.

Recovery is attempted only when a frame is already known to be damaged and its
reorder window has elapsed, or a later frame has overtaken it. A stream losing
nothing never runs the decoder at all.

How much parity a frame carries is the sender's choice and is not announced:
every parity packet says what its own frame's block is, which is all a receiver
reads. The rule the host applies is a percentage of the frame's datagram count,
rounded up, raised by half for a key frame, with a floor of 5% and a ceiling of
50%. At the 2.79% loss measured in [`findings.md`](findings.md) that is 20% on a
delta frame and 30% on a key frame, and the measurement there puts the stream at
99.9% of frames delivered for **less** total bandwidth than the
key-frame-on-demand recovery it replaces.

The level is meant to follow measured loss, and the receiver report in
[`congestion.md`](congestion.md) is where that measurement will come from. That
message does not exist yet, so a session runs at the measured level and the
level is a settable parameter for the congestion work to drive.

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

### Padding carries no picture and exists to be timed

A datagram with bit 2 of `flags` set carries the ordinary sixteen-byte header
and a payload of zeroes. It exists so that the host can measure a rate the
picture is not currently supplying, which is most of the time: a still desktop
produces no frames at all, and a congestion controller that only learns from
media never learns that the link has recovered.

A receiver counts its sequence number, records its arrival for the next
`Feedback`, and discards it. `frame_id`, `capture`, `slice_pkts`, `pkt_idx` and
`slice_id` mean nothing on a padding packet and must not reach reassembly.
Sequence accounting is the only thing padding takes part in, and it is the only
thing it is for.

`slice_id` 255 is what forward error correction took, and it is specified above.
The spare flag bits were not needed: one byte at a fixed offset separates the
two kinds of datagram, and a second marker saying the same thing would be a
second thing to get wrong. They stay spare.
