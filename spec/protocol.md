# tachyx wire protocol — draft 4

Status: draft. Covers the session: what two devices say to each other once they
have met. How they meet is [`pairing.md`](pairing.md), and how they find each
other when they are not on one network is [`relay.md`](relay.md). A relayed
session is this session, unchanged: the relay forwards packets it cannot read.

Draft 1 defines `slice_id` 255, which draft 0 reserved. A draft 0 client reads a
parity packet as a slice of the picture and then completes no frame at all, so
the version had to move rather than the field being added quietly.

Draft 2 adds the set of codecs a client can decode to `ClientHello`. `postcard`
writes no field names, so a draft 1 hello read as a draft 2 one ends a field
short and a draft 2 hello read as a draft 1 one leaves a byte over. Neither
fails to parse on its own, which is what the version exists to catch.

Draft 3 moves the cursor report into the logical pixels the compositor measures
the pointer in, and gives each pointer picture an identifier. A report from
either side read by the other places the pointer by a fifth of the screen and
draws it at the wrong size.

Draft 4 adds sound. A datagram now carries audio as well as picture, marked by a
flag bit draft 3 left spare, and a draft 3 receiver ignores a spare bit rather
than refusing it: it reads an audio datagram as a picture packet that describes
no fragments, rejects it as malformed, and never records its sequence number. A
hundred audio packets a second then read as a hundred lost datagrams a second,
which drives parity to its ceiling and the bitrate to its floor. `HostHello`
gains a field in the same draft, with nothing in the encoding to name it.

## Transport

A single QUIC connection carries everything.

| Path | QUIC object | Reliability |
|---|---|---|
| Video and audio | DATAGRAM (RFC 9221) | unreliable, never retransmitted |
| Control and input | one bidirectional stream, opened by the client | reliable, ordered |

Video never shares a stream with control. A lost video packet must not delay an
input event, and an input event must not delay a frame.

### Security

Both ends present a self-signed certificate derived from a key they keep, and
both ends check the exact key. The key **is** the device identifier and there is
no certificate authority. [`pairing.md`](pairing.md) is where that is specified;
this section says only what it means for the session.

- The client accepts one key, the one it paired with. Nothing is pinned on first
  sight.
- The host requires a client certificate and refuses a device that is not in its
  paired list. Before pairing existed it accepted anything that could reach the
  port.
- Validity dates and subject names are not examined by either end. The
  certificate is self-signed, so a forger writes what it likes into both, and the
  key is the only thing an attacker has to break.
- A certificate whose key is not Ed25519 is refused before it is compared, rather
  than compared and found different.

The identity travels no further than the handshake. Which device connected is
what the host reads its permissions from; the messages below say nothing about
who is at either end.

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

The datagram path does not use this encoding, or serde at all. See below.

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
  The bitrate ceiling is re-read along with the viewport, unlike the codec set:
  a client folded into a picture-in-picture thumbnail caps the stream low and
  restores it on the way back, and a cap moved on the client's settings screen
  reaches the live session the same way.

`InputBatch`
: A batch of input events, each with the client's monotonic timestamp. Batched on
  a short tick rather than sent individually, so a burst of pointer motion costs
  one packet.

`Feedback`
: The receiver report that drives congestion control. When each datagram
  arrived, which never did, and cumulative counts of both. Specified below and
  in [`congestion.md`](congestion.md).

`KeyframeRequest`
: Sent when reassembly failed and the client cannot continue from what it has.

`Audio`
: What to do about the host's own sound: set the mode, set the volume, set the
  mute. Sent only to a host whose hello said it has audio. Specified below.

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
: The protocol version, and whether the host can stream its own sound. There is
  nothing else for the host to announce at this point: the host decides how to
  encode, not the client, and it says so in `StreamConfig` once it has read what
  the client can drive. Encoders, capture formats and the list of monitors join
  it when choosing a monitor is a thing a client can do.

  Audio belongs here because it is the one thing a client has to know before it
  draws its own controls, and it is probed on the host's own machine the way the
  codec sets are: a sound server that answers, and an encoder element that is
  installed. A control that is offered and then does nothing is worse than one
  that is not offered, because the user cannot tell it from a fault.

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

`Audio`
: What the host is doing about sound, and what its sink is set to. Sent after the
  hello, as the answer to every `Audio` command, and whenever the sink changes
  underneath the session. Specified below.

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

## Sound

The host streams what its own speakers would have played. There is no separate
mixer and no per-application selection: the capture tap is the default sink's
monitor, which is everything the machine is playing.

```rust
pub enum AudioMode {
    /// Nothing captured, nothing streamed, the host's output untouched.
    Off,
    /// Streamed to the client, and the host's own sink muted.
    Exclusive,
    /// Streamed to the client, and the host goes on playing it too.
    Shared,
}

pub enum AudioCommand {
    SetMode(AudioMode),
    /// Permille of the sink's own 100%.
    SetVolume(u16),
    SetMuted(bool),
}

pub struct AudioStatus {
    /// What the host is doing, which is not always what it was asked to do.
    pub mode: AudioMode,
    /// The sink's volume, in permille.
    pub volume: u16,
    /// Whether the sink itself is muted.
    pub muted: bool,
}
```

`Exclusive` is the mode a user in another room asks for: the sound follows them
rather than staying with the computer. It works because the tap is ahead of the
sink's own volume and mute, on the monitor ports, so muting the sink silences
the host without touching a byte of what is streamed.

`Off` is where a session starts and where it must end. The host's mute is
restored when the mode leaves `Exclusive`, when the client disconnects, and when
the daemon stops. Nobody sitting down at the host afterwards should have to work
out why their machine is silent.

The status reports what the host **is** doing rather than what it was asked to
do. A host that cannot start capture, or that refuses a mode, says so by
reporting the mode it is really in, and the client's control follows the report
rather than its own last click.

### One volume, and the client cubes it

The host sink's volume is the volume. The client has no gain control of its own,
and this is a consequence of where the tap is rather than a simplification: the
monitor ports carry the same samples whatever the sink's volume is set to, so a
client that turned its own knob would change what it hears while the host's
control said something else.

So the host applies `SetVolume` to its sink, reports it back, and the client
turns the reported number into playback gain:

```
gain = (volume / 1000)^3
```

Cubed because permille here is a position on a control and not a ratio of
amplitude. PulseAudio, and the pipewire-pulse server that answers for it, cube
the control to reach amplitude, so half way along the control is an eighth of
the amplitude. A client that applied the number directly would be louder than
the host at every setting but the two ends.

The same one knob then works in both streaming modes. In `Shared` it moves the
host's speakers and the client's playback together, which is what a single
volume means. In `Exclusive` the sink is muted, so it moves only what the client
hears, and the number it moves is still the host's.

A volume above 1000 is clamped by whichever end reads it. The encoding cannot
forbid one, and what reads it is a cube: a peer sending ten thousand would
otherwise put a thousandfold gain into a listener's ears.

`muted` is the host's own silence and only the host's. It reads true throughout
`Exclusive`, while the client is hearing everything. A client that applied it as
silence would hear nothing in the one mode chosen to make it hear everything.

The status is sent whenever the sink changes underneath the session, including a
change somebody made at the host's own keyboard. Without that, the two ends
disagree about the volume until one of them is touched again.

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

## Datagram path

Every datagram carries a fixed 16-byte header written by hand, byte by byte. At
thousands of packets a second, and as the part of the protocol most worth
auditing, the layout is specified here rather than derived from a Rust type.

All fields are little-endian.

A datagram is one of four things, and two flag bits and one byte tell them
apart. Bit 3 of `flags` marks audio and bit 2 marks padding: neither belongs to
a frame of picture, so both are read before any field that describes one. What
is neither is picture or parity, and `slice_id` says which of those it is: 255
means parity, anything else is a slice of the picture. All three are at fixed
offsets, so telling the four kinds apart costs three comparisons and no parsing.

Bits 2 and 3 together are refused rather than resolved by precedence. They say
opposite things about the payload, one carrying sound and the other defined to
carry nothing, and a receiver that guessed would play whichever the sender did
not mean.

Two fields mean the same thing in all four:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 8 | 2 | `seq` | Session-wide datagram counter, so a gap is visible across a frame boundary. Compared circularly, in the manner of RFC 1982. Parity, padding and audio are counted in it like any other: they cost the path the same bytes and their loss is the same evidence about it. |
| 15 | 1 | `flags` | Bit 0 marks a key frame, bit 1 the frame's last slice, bit 2 padding, bit 3 audio. The rest are spare and a receiver must ignore them. Bits 0 and 1 are meaningless on anything but a picture packet. |

Two more mean the same thing in picture and parity:

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | `frame_id` | Reassembly key, and what a recovery request names. Counts encoded pictures and skips none. |
| 14 | 1 | `slice_id` | Which slice of the frame, or 255 for parity. |

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

### Sound travels on the same datagrams

A datagram with bit 3 of `flags` set carries exactly one Opus packet.

| Offset | Size | Field | Purpose |
|---|---|---|---|
| 0 | 4 | `frame_id` | Counts the session's audio packets and skips none, for the reason the picture's identifier skips none: a packet every trace of which was lost leaves nothing but the hole to say it existed. A receiver reads that hole as the sound it has to conceal. |
| 4 | 4 | `capture` | When the samples were captured, on the same clock and in the same 100-microsecond ticks a picture's capture time is in. So the two can be lined up without either end sending a mapping between two timelines. |
| 10 | 5 | | Zero on write, ignored on read. |

There is no length field, no channel count and no sample rate. An Opus packet
describes its own duration and channel layout in its own table of contents, and
the datagram's length is the packet's, so a second statement of any of it would
be a second thing to get wrong.

Those five zero bytes are the opposite of what padding writes there, and that is
deliberate. `slice_pkts` of zero is not a slice, so a receiver that does not know
the audio bit rejects the datagram by name instead of handing a video decoder a
band of picture it read out of an Opus packet. It is the cheapest refusal
available and it costs the sender nothing.

Sound takes part in the sequence accounting and in nothing else. It reaches no
reassembly: one Opus packet is one datagram, so there is nothing to put back
together, and every field that would say which picture it belongs to is absent
rather than meaningless. It is counted in `seq` for the same reason parity and
padding are: it costs the path the same bytes, its loss is the same evidence
about the path, and a receiver report that skipped it would omit a tenth of the
session's datagrams from every delay measurement taken.

It is not paced with the picture, and the host must not queue it behind one. A
key frame is released over tens of milliseconds, and sound queued behind one
arrives that late every time a key frame goes out. A gap in the picture is a
frame nobody sees; a gap in sound is a click everybody hears.

### Why sound takes a flag bit

`slice_id` 255 is what forward error correction took, and the next identifier
down would have cost the picture a slice it can use. That is the small reason.

The real one is that `slice_id` is read **after** the fragment fields, and on an
audio datagram those fields describe nothing. Marking sound the way parity is
marked would mean validating a fragment count and index that the sender never
wrote, on every audio packet, before finding out that it is not a picture. The
flag bit is read first, exactly as padding's is, which is what lets a datagram
that belongs to no frame skip every check about frames.

Bits 4 through 7 stay spare, and a receiver must ignore them.
