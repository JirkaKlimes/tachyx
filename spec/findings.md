# Platform findings

What was measured on the target machine, and which assumptions in
[`architecture.md`](architecture.md) did not survive contact with it.

Target: NixOS 26.05, Hyprland 0.56.2, AMD Radeon 780M at `/dev/dri/renderD128`
(Mesa 26.1.2, VA-API 1.23), NVIDIA RTX 4060 Laptop, GStreamer 1.26.11.

Everything below was executed, not read. Where something is quoted from
documentation rather than run, it says so.

## Assumptions that were disproven

These are the lines in the architecture document that are wrong for this target.
They are recorded here rather than edited out of that document, because the
document is the original design and the difference is the interesting part.

### Damage regions do not exist in practice

The architecture says capture is "DMA-BUF frames + damage regions" with "damage
awareness: static screen implies near-zero bitrate". Hyprland 0.56.2 reports
full-frame damage on every frame, on both `ext-image-copy-capture-v1` and
`zwlr-screencopy-v1`, 400 frames out of 400, even when the client's own
`damage_buffer` hint was a single pixel.

Idle suppression arrives from somewhere better instead. `capture()` blocks until
the screen changes, so a static desktop produces no frames at all rather than
cheap ones. Idle inter-frame time measured at 91 ms median with a 1.53 second
maximum. The damage handler is still wired, because it costs nothing and a later
compositor may narrow it, but nothing depends on it.

### Portal RemoteDesktop does not exist on this compositor

The architecture names "portal RemoteDesktop (libei)" as the default input
injection path, with the wlroots protocols as a fallback for compositors that
support them. On this host it is the other way around and there is no fallback:
`xdg-desktop-portal-hyprland` 1.4.1 declares Screenshot, ScreenCast,
GlobalShortcuts and InputCapture, and `org.freedesktop.portal.RemoteDesktop` is
absent from the bus. The wlroots protocols are the only road.

### Intra-refresh and long-term reference frames are unavailable

The architecture specifies "loss recovery without retransmit: intra-refresh
(rolling I-columns, no IDR spikes) + long-term reference frames — on loss, client
requests a P-frame off the last acked LTR".

Neither is exposed. `vah264enc`'s complete property set contains no intra-refresh
and no LTR control, and neither does `nvh264enc`. There is no GStreamer path to
either on either GPU in this machine. Every recovery is therefore a full IDR, and
bandwidth headroom has to be budgeted for IDR spikes rather than designed away.

The escape hatch, if this ever blocks: VA-API exposes an intra-refresh parameter
that the GStreamer element does not surface, reachable through `cros-libva`. That
is a much larger undertaking and is not being started now.

### The portal capture path is not a latency improvement

The architecture presents `xdg-desktop-portal` ScreenCast as the default and the
direct wlroots protocol as a second backend. On Hyprland the portal is strictly
worse: `xdg-desktop-portal-hyprland` is itself a `zwlr-screencopy` client that
republishes frames into PipeWire. Taking the portal means the screencopy path plus
D-Bus negotiation, an extra process, and at least one extra buffer handoff per
frame. It also loses the render-node identification and modifier list that the ext
protocol supplies, and the separate cursor metadata channel.

## Capture

`ext-image-copy-capture-v1` with `ext-output-image-capture-source-v1`, DMA-BUF,
no portal. Both are advertised by Hyprland 0.56.2, alongside `zwlr-screencopy-v1`
version 3 as a fallback for compositors without the ext protocol.

Measured on eDP-1 (2880x1800 at 120 Hz): 123.6 frames per second under motion,
8.3 ms median between frames, refresh-locked. `presentation_time` is
`CLOCK_MONOTONIC` and lands 1 to 4 ms in the past at the moment the frame is
ready.

The cursor is excluded from the frame unless asked for, and a separate cursor
session supplies position, hotspot and a 48x48 ARGB image. This is exactly the
client-side cursor rendering the architecture asks for, at no cost.

Direct capture requires no user consent on Hyprland. Anything that can reach the
Wayland socket can record the screen. That is a property of the compositor, not of
this daemon, but it belongs in the security notes.

### Traps

The render node must be read from the capture session's `dmabuf_device` and
resolved by matching device numbers across `/dev/dri`. On this machine `card0` is
the NVIDIA device while `renderD128` is the AMD one: the card index and the render
node index are crossed, so deriving one from the other picks the wrong GPU.
Allocating on the NVIDIA node fails immediately with `EINVAL`, which is at least a
loud failure.

`DRM_FORMAT_MOD_INVALID` appears in the advertised modifier list and must be
removed before the list is offered to the allocator.

A single-plane `XR24` buffer reports three planes under the AMD compression
modifiers, because the compression metadata occupies planes of its own. Every
plane must be added to the buffer parameters; assuming one plane fails.

A session permits one capture in flight. Concurrent sessions on one output do
work, if overlap is ever needed.

## Encode

`vah264enc` on the AMD integrated GPU. Encoder latency, sink pad to source pad:
3.8 ms at 720p, 7.3 ms at 1080p (7.4 ms at the 99th percentile), 11.1 ms at 1440p.

NVENC also works on this machine and has richer latency controls, but feeding it
means moving frames from the integrated GPU, where the compositor renders them, to
the discrete GPU across PCI Express, which costs a copy and wakes a power-hungry
device on a laptop. It stays available as a second backend, one element name away.

`vah264lpenc` does not exist here: the low-power encode entrypoint is an Intel
feature, and this driver exposes only the standard one.

### Constraints that shape the rate controller

A bitrate change forces an IDR. This is not incidental; the property setter marks
the encoder for reconfiguration and the reconfiguration resets encoder state.
Congestion control must therefore be coarse: bitrate updates are rate-limited and
applied on a significant change, never per frame.

The coded picture buffer has a floor of half the bitrate, which is 500 ms of
buffering. Smaller values are silently replaced by *twice* the bitrate, which is
2000 ms, and that is also the default. It must be set explicitly on every
reconfiguration.

More slices per frame makes latency worse, not better: 7.30 ms mean at one slice
against 8.81 ms mean and 23.74 ms maximum at four, because the hardware encodes
the whole frame either way. Slices are for keeping NAL units near the datagram
size and for loss resilience, and are never a latency optimisation.

### Live reconfiguration

Verified on one running pipeline with no teardown and no dropped frames.

Bitrate changes take effect immediately, at the cost of the IDR noted above.

Resolution changes are made by setting new caps on a filter placed between the
scaler and the encoder. Measured: the change takes 0.03 milliseconds to apply,
takes effect on the very next frame, drops nothing, and the encoder renegotiates
its level in place. The source keeps pushing frames at the monitor's own size and
never changes its caps, which is the only thing a fixed-size capture source can
do.

An earlier revision of this document claimed the opposite, that a downstream
filter stalls the pipeline and the change must come from the source. That is
wrong. It was measured on a pipeline whose source could renegotiate its own size,
which capture cannot.

A forced keyframe takes effect on the very next frame, and re-sends the parameter
sets with it. The limit on how often one may be forced defaults to none.

### Constant bitrate spends most of the stream on padding

Measured on synthetic desktop content at 1080p60 with the property set above:

| Content | Total | Filler | Picture |
|---|---|---|---|
| Static page with a moving cursor | 4.75 MB | 87.2% | 0.76 Mbit/s |
| Full-page scrolling | 4.75 MB | 78.2% | 1.45 Mbit/s |
| A busy quarter-screen window | 5.18 MB | 0% | 7.86 Mbit/s |

In constant-bitrate mode the encoder pads every frame to the target with filler
data. A quiet desktop therefore spends 8 Mbit/s to carry 0.76 Mbit/s of picture.

The waste is the smaller problem. The larger one is that the transport's bandwidth
estimator only refreshes while the sender is not limited by its own supply, so a
stream that is seven-eighths padding looks like genuine demand and the estimate
degenerates into a measurement of our own padding.

Variable bitrate with a target percentage emits no filler at all and tracks
content: 1.95, 1.98 and 6.20 Mbit/s for the same three cases.

### The frame rate in the caps is the real bitrate control

The encoder budgets bits per frame as the bitrate divided by the frame rate
declared in the caps, and ignores presentation timestamps entirely. Delivery
cadence does not enter into it.

So caps declaring 60 while capture delivers 20 produces a third of the configured
rate, and caps declaring 30 while capture delivers 120 produces four times it.
Bitrate and frame rate are therefore one setting, not two, and the declared rate
must track the output's refresh rate rather than a nominal figure.

### The scaler does not convert frame rate

Asking it to is a negotiation failure, not a conversion: a pipeline placing a
different frame rate downstream of the scaler refuses to link at all.

So the two halves of the encoder's bit budget are set in different places. The
declared frame rate belongs on the source caps, and the filter between the scaler
and the encoder carries geometry alone.

### Coded size is not always the size that was asked for

The encoder codes 1080 as 1088 and 1800 as 1808, attaching a crop. 720 and 1440
need no crop. A client that ignores the crop shows eight rows of encoder working
data and a slight vertical squeeze.

The scaler does not preserve aspect ratio unless told to, and when it corrects for
that it records the correction in a form that never reaches the bitstream: the
sequence parameter set of a 16:10 capture scaled to 16:9 declares no aspect ratio
at all. A client cannot recover the geometry from the stream, so the coded size,
the visible size and the display aspect all travel in the stream configuration
message.

### What the encoder tells a decoder about buffering

The sequence parameter set declares that the stream reorders nothing, which is the
property that matters: a decoder outputs each picture as soon as it is decoded.
It also declares room for two frames in the decoded picture buffer, and no
encoder setting changes that. Sizing the buffer does not delay output, so the
requirement is that reordering is zero, and that is met.

### The encoder imports one buffer layout, not any

The scaler accepts exactly one memory layout per pixel format for shared buffers,
the compressed one this GPU prefers, and does not accept a plain linear layout.
Capture must therefore choose its buffer layout from the intersection of what the
compositor offers and what the encoder will import, rather than from the
compositor's list alone. Negotiating the two independently produces an import
failure at run time on any machine whose lists differ.

Restating the input size on a filter that does not change it is not free: it
defeats the scaler's pass-through and doubles its cost, from 9.2 to 18.6
milliseconds at full resolution. The filter is set only when the encode size
genuinely differs from the capture size.

## Input injection

`zwlr_virtual_pointer_manager_v1` version 2 and `zwp_virtual_keyboard_manager_v1`
version 1. Injection was proven to reach the compositor's input path with a
controlled experiment using the idle notifier as the observable.

Absolute pointer motion maps onto the bounding box of the whole output layout, not
onto one monitor, and that box does not begin at the origin. On this two-monitor
layout it spans x from 843 to 5043 and y from 0 to 2940. The extents must be
computed from logical output sizes.

There is no high-resolution scroll: the virtual pointer protocol has no
`axis_value120` request at any version. Continuous axis values and discrete
notches are what exist.

The compositor does not derive modifier state from injected key events. Modifier
state is tracked by the daemon and sent explicitly.

An uploaded keymap must name its group, or the compositor fails to read it. The
keymap is passed over a memory file descriptor, must be terminated with a zero
byte, and the length given to the compositor must include that byte.

### Two virtual keyboards, not one

Physical keys must have the host's layout applied, which is the entire reason keys
travel as HID usage codes. Text from a phone's on-screen keyboard must bypass
layout entirely, because the character it produced has no relationship to any key.
One keymap cannot serve both without live surgery on the layout the focused
application is using.

So there are two virtual keyboards on the seat. One carries the host's real layout,
read from the seat itself, and is never re-uploaded. The other carries a scratch
keymap for text. XKB accepts a Unicode keysym directly, verified for a currency
symbol, a CJK ideograph and an emoji outside the basic planes, so producing a
character is a matter of naming its codepoint. Re-uploading a keymap pushes an
event to the focused application and some applications reset input method state on
it, so the scratch keymap accumulates the codepoints seen so far and is re-uploaded
only when a genuinely new one appears.

## Transport

QUIC datagrams for video, one reliable stream for control and input.

The congestion controller applies to datagrams. This was checked in three places
in quinn's source and confirmed by offering 5000 datagrams at once: datagram
packets are acknowledgement-eliciting, gated by the congestion window, counted in
flight and tracked for loss, but never retransmitted. So the bandwidth estimate
comes free from the transport, and a delay-based estimator of our own is not
needed on a local network.

A successful call to send a datagram does not mean the datagram was sent. Under
pressure quinn discards queued datagrams to make room. In the burst test 2166 of
5000 vanished with no error reported. The send buffer is therefore sized so that
a whole key frame fits it, and the space is checked before a frame is queued, so
that a frame is dropped whole, by us, rather than shredded by the transport.

### Two things this document said about that are wrong

An earlier revision said quinn "discards arbitrary ones". It discards the
**oldest first**, in order, which is worse rather than better for us: the oldest
queued datagrams are the head of the frame currently going out, so the frame that
loses its pieces is the one the client is about to try to decode.

It also said the send buffer is "sized to about two frames". Two frames of what
decides whether the stream starts at all. The two frame sizes measured here are
130,427 bytes in 92 datagrams for a 1080p key frame and 16,673 bytes in 12
datagrams for a P frame at 8 Mbit/s and 60 Hz. Read as two P frames it is 33 KiB,
into which no key frame ever fits, so every key frame is refused, the client never
gets one to synchronise on, and no picture is ever shown. The figure is one key
frame scaled by pixel count, charged the per-datagram accounting below, and given
half a key frame of headroom: 203,785 bytes at 1920x1088.

### Reaching quinn's eviction path is fatal, not lossy

Measured, in both profiles, against quinn-proto 0.11.17.

The eviction loop in `Datagrams::send` subtracts a dropped datagram's length from
`payload_bytes` twice: once inside `DatagramBuffer::pop_front`, which the loop
calls, and once again in the loop body. The count then no longer describes the
queue.

- Debug, or any build with overflow checks: the next drain of the queue subtracts
  a real length from the understated count and panics with `attempt to subtract
  with overflow` at `connection/datagrams.rs:203`. The panic poisons the
  connection's mutex, so every later use of that connection panics too, and
  dropping it panics during cleanup, which aborts the process.
- Release: `payload_bytes` wraps instead. The connection then reports zero free
  space for ever, so the gate refuses every frame and the video stops with no
  error at all. The next datagram offered reaches quinn's own
  `expect("datagrams.outgoing.payload_bytes desynchronized")` and panics there.

So the gate is not a refinement of quinn's behaviour under pressure. It is what
keeps the connection alive. This was found by running the transport at
saturation, where it killed the connection, and reproduced deterministically:
fill the queue past the eviction threshold, then let it drain.

### The free-space report understates, and by how much

`Connection::datagram_send_buffer_space()` reports the buffer less the payload
bytes queued less one entry's worth of accounting. The eviction loop it is meant
to predict charges one entry's worth **per queued datagram**, so the two disagree
by 32 bytes for every datagram already in the queue.

Measured with a 1080p key frame queued: 92 datagrams held, 71,854 bytes reported
free, of which the report says another 49 datagrams fit and the queue really
takes 47. Trusting the report evicts, and evicting is fatal.

The gate closes the gap without guessing. This transport is the only producer on
that queue and quinn takes from the front, so the datagrams still in it are the
tail of the ones pushed, and the tail whose sizes add up to the payload bytes
reported is the only tail it can be. That gives the entry count the report leaves
out. When the sizes cannot be made to add up, something removed a datagram from
the middle, and the gate refuses everything until the queue drains and the record
squares itself again.

### The receive buffer evicts the same way, and its default is not the trap

`DatagramState::received` discards the oldest queued incoming datagram to make
room, silently, exactly as the send side does. Its accounting is correct, so
nothing panics; the datagrams simply vanish.

Measured: a client whose datagram receive buffer is 29,991 bytes receives 20 of
the 92 datagrams of a key frame sent to it. The frame is destroyed on the client,
after a healthy network delivered every packet, with no error anywhere.

quinn's default is 1,250,000 bytes, which is fine. Any configuration that lowers
it below a key frame plus the application's drain lag is not. Send and receive are
therefore set to the same figure. With both at 203,785 bytes, a key frame that
sits unread for 100 ms while the application is busy arrives complete: 92 of 92
datagrams, nothing discarded.

### Loopback, measured

180 frames at 60 Hz, one key frame every 60, real sockets, real handshake,
1418-byte payloads.

| | |
|---|---|
| Delivered | 180 of 180, 2400 datagrams, none evicted, none lost |
| Carried | 26.74 Mbit in 2.985 s, 8.96 Mbit/s |
| Capture to delivery | 1.42 ms median, 2.56 ms at the 99th percentile, 2.80 ms worst |
| Round trip | 193 µs, quinn's own path estimate |
| BBR pacing rate | 670 Mbit/s |
| Congestion events | 0 |

The same link driven as hard as the gate allows for 1.5 seconds: 34,850 frames
offered, 30,357 queued, 4493 refused whole by the gate, 2.99 Gbit/s carried,
33 µs round trip, BBR pacing rate 3.39 Gbit/s. Of 397,373 datagrams received,
4911 were lost, which is the kernel dropping UDP at three gigabits and not the
transport evicting: eviction would have killed the connection outright.

Capture to delivery is measured from when the frame was due to be sent, so it
includes the sender's own scheduling and the encoder is not in it. The pacing
rate is only ever `Some` under BBR. Cubic and NewReno report `None`, so choosing
BBR is the precondition for having a bandwidth estimate at all rather than a
preference between controllers.

Datagram frames are written into a packet before stream frames, so control traffic
cannot starve video within a packet, though the two still share the window.

The default transport settings are wrong for a local network by large factors. The
initial round trip estimate defaults to 333 ms, which seeds loss detection, so a
lost packet is not suspected for a third of a second on a link where the real
figure is under one millisecond.

Read from quinn 0.11.11 and quinn-proto 0.11.17 rather than run, for the record:
`max_idle_timeout` 30 s, `initial_rtt` 333 ms, `initial_mtu` and `min_mtu` 1200,
no keep-alive, no acknowledgement frequency configuration, `datagram_send_buffer_size`
1 MiB, which is about eight 1080p key frames of queued latency, and Cubic as the
congestion controller. `AckFrequencyConfig::max_ack_delay` is clamped by quinn to
at least the peer's minimum and at most the greater of the path round trip and
25 ms, so the 2 ms asked for here is honoured on a local link and not elsewhere.

There is no public API that fails fast instead of shredding.
`Connection::send_datagram` always passes `drop = true`, so its `Blocked` result
is unreachable. The path that does not drop is reachable only through
`send_datagram_wait`, which waits rather than failing, and waiting prioritises old
datagrams over new ones, which for live video is exactly backwards. The gate is
the only defence.

quinn defaults to one cryptographic provider and rustls to another, so accepting
both defaults compiles both. The provider is selected explicitly, and with quinn
on `rustls-ring` and rustls on `ring` the resolved lockfile contains no
`aws-lc-rs` at all.

Recovery is on demand rather than by forward error correction. Video datagrams are
never retransmitted, and neither intra-refresh nor long-term reference frames are
reachable on this hardware, so what recovery means here is exactly one thing: the
client asks for a key frame and the host sends an IDR. Sunshine and Moonlight carry
a fifth of their bandwidth as parity because a wide-area round trip exceeds a frame
period. On a local network the round trip is under a millisecond, so parity on every
frame would be paying continuously for something that costs one round trip and one
IDR when it happens. The header reserves room for parity for when this leaves the
local network.

That makes detecting loss the whole of the recovery mechanism, and one kind of loss
leaves no evidence to detect: a frame that loses every packet. Nothing in the
sequence numbers separates it from the frame boundary around it. The frame
identifiers are what say it happened, which is why the host must number every
picture it encodes and skip none. That rule and its consequences are in
[`protocol.md`](protocol.md).

## Discovery

The host advertises over mDNS. On Android the client uses the platform's own
service discovery, which acquires the multicast lock itself for foreground
applications, so the application needs no permission for it. Requesting the
multicast permission directly would add an implied hardware requirement that
filters the application out of the store listing for devices without Wi-Fi, and a
held lock keeps the radio waking on every beacon interval.

## Client

There is no prior art. No Flutter Moonlight or Sunshine client exists, and every
Flutter video plugin surveyed assumes a URL or a file as its source. RustDesk is
the closest working system and takes the opposite approach to ours: on Android it
decodes into a byte buffer, converts on the CPU, and rebuilds an image in Dart
every frame.

The decode and present path is therefore different on each platform, and the split
is forced by measurable costs rather than taste.

### Android

Decode with the platform decoder through the `ndk` crate, whose media API is safe
and was compiled for `aarch64-linux-android` under `forbid(unsafe_code)`. It
decodes straight into a surface, so no frame ever passes through our code.

Presentation uses Flutter's newer surface producer, reached through about sixty
lines of Kotlin. On current Android that path hands the decoder's buffers to the
compositor's Vulkan backend directly. The alternative, `irondash_texture`, uses
the older surface texture API, and Flutter's own source describes the resulting
OpenGL-to-Vulkan interop as costing one device-to-device copy per frame and
existing only as a migration fallback. Sixty lines of Kotlin is the cheaper price.

One unavoidable unsafe call: turning the Java surface object into something the
decoder can use. Nothing in the type system can prove a Java object reference is a
live surface. It is isolated in a leaf crate, as agreed for the host's descriptor
import.

### Linux

Decode with GStreamer, which is the same stack the host already uses to encode, so
the dependency is already paid for. The decoded frame is uploaded and converted on
the GPU by existing GStreamer elements and handed to Flutter as a texture, so we
write no graphics code.

Flutter's Linux embedder has no interface for accepting a shared buffer directly,
and no proposal to add one. It works anyway, because the callback that supplies a
texture runs on the rendering thread with Flutter's own graphics context already
current. Two calls hand raw display and context handles across that boundary, and
they are the only unsafe on the Linux client.

The client must run on Wayland. Under X11 the embedder reads the entire window
back from the GPU every frame before presenting, which is several megabytes and a
pipeline stall per frame, entirely independent of anything we do.

This is the riskiest part of the whole project. The Linux texture path depends on a
crate whose graphics payload has roughly eleven thousand lifetime downloads, a
single maintainer, a stable release from 2023, and not one issue ever filed about
its OpenGL support, working or broken. It is about a hundred lines and we should be
prepared to take ownership of it.

### Consequences for the host

Three requirements land on the encoder, and all three are cheap if done from the
start and expensive to retrofit.

Encoded dimensions must be multiples of sixteen. Flutter's Android present path
does not apply the crop rectangle that H.264 uses to express sizes like 1080, which
the codec must round up to 1088. Every rung of the resolution ladder is therefore
chosen from multiples of sixteen.

The parameter sets are repeated in the stream before every key frame rather than
sent once out of band. The client then needs no separate configuration step,
survives a reconnect and a resolution change without one, and recovers by itself if
a parameter set is lost on a path that does not retransmit.

The sequence parameter set must declare that the stream reorders nothing. Without
it a decoder buffers frames before emitting them, which adds latency that no client
setting can remove. We control the encoder, so we state it rather than rewriting
the bitstream on the client the way Moonlight has to.

### Input

Flutter's built-in gesture recogniser cannot be used for tapping. Asking it for
both a single and a double tap makes it hold the gesture arena until the
double-tap timer expires: a measured 310 milliseconds before a single tap is
reported, and no single-tap callback at all when a double tap does happen. The
client uses one raw pointer listener and its own state machine, which reports a
tap the instant the finger lifts.

Keys are read before any text field can consume them, which is also what stops the
back gesture from leaving the session and stops tab and the arrows from moving
focus instead of reaching the remote desktop.

Text from the on-screen keyboard is taken as edit deltas rather than by watching a
hidden text field change, so composition in a language that needs it arrives as
one committed string rather than as a stream of half-formed guesses.
