# Platform findings

What was measured on the target machine, and which assumptions in
[`architecture.md`](architecture.md) did not survive contact with it.

Target: NixOS 26.05, Hyprland 0.56.2, AMD Radeon 780M at `/dev/dri/renderD128`
(Mesa 26.1.2, VA-API 1.24 with libva 2.24.0), NVIDIA RTX 4060 Laptop,
GStreamer 1.28.6. Two outputs: eDP-1 at 2880x1800 and 120 Hz, DP-2 at 3440x1440
and 74.983 Hz.

An earlier revision of this line said VA-API 1.23 and GStreamer 1.26.11. The
development shell now supplies 1.24 and 1.28.6, so any figure below that is not
dated to a re-measurement was taken on the older pair.

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
cheap ones. On an output holding an empty workspace, capture yields 37 frames in
30 seconds, 1.23 per second, with about one second between them, and the whole
process spends 10 ms of processor time over that window, which is 0.03 percent
of one core. It blocks; it does not spin. An earlier revision recorded 91 ms
median between idle frames, which was an output that still had something small
moving on it. The damage handler is still wired, because it costs nothing and a later
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

Re-measured with the crate that does this for real, `tachyx-media`, rather than
with a probe.

On DP-2 (3440x1440 at 74.983 Hz) under continuous motion: 600 frames in 8.000
seconds, 75.0 per second, refresh-locked. The consumer waits 13.33 ms between
frames at the median, 13.62 at the ninetieth percentile and 15.10 at worst.
Nothing dropped, nothing failed.

`presentation_time` is `CLOCK_MONOTONIC`, and it lands 0.04 ms in the past at the
median and 0.09 ms at worst, not the 1 to 4 ms an earlier revision of this
document recorded. Hyprland stamps the copy, not the scan-out, so the field is
not a measure of how long the picture has been on the glass.

### eDP-1 could not be made to present under motion

The 123.6 frames per second an earlier revision recorded for eDP-1 did not
reproduce. With the laptop panel connected, enabled, lid open, DPMS on, holding
its own workspace, and a `videotestsrc` window running full screen on it at 144
frames per second, capture of eDP-1 yields 2.7 frames per second, with waits of
about one second between them. The same binary on DP-2, at the same moment,
yields 75.0. Hyprland is not repainting that output, and neither VRR being on for
it nor `solitary` nor direct scan-out explains it: both are reported blocked.

So the refresh-locked claim holds and is measured, but on DP-2. Nothing is known
about capture at 120 Hz on this machine, and the 8.3 ms figure should not be
relied on until eDP-1 can be made to present again.

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

A single-plane `XR24` buffer can report several planes under an AMD compression
modifier, because the compression metadata occupies planes of its own, so every
plane must be added to the buffer parameters.

Under the one modifier this pair actually negotiates, `0x0200000010401b04`, it
does not. Delta colour compression is off in that modifier, bit 13 clear, so
there is no metadata plane and the allocator reports exactly one plane: offset 0,
stride 11776 on eDP-1 and 13824 on DP-2. An earlier revision said three planes
without naming a modifier. The loop over planes stays, because it costs nothing
and the count comes from the allocator rather than from an assumption, but the
common case is one.

A session permits one capture in flight. Concurrent sessions on one output do
work, if overlap is ever needed.

### The compositor's buffer imports into the encoder

This was the one integration nothing had exercised, and it works. A buffer object
allocated by GBM on the render node the session named, filled by the compositor,
handed to GStreamer as a single memory covering the whole object with a
`GstVideoMeta` carrying the plane offsets and strides, imports into `vapostproc`
and reaches `vah264enc`. The output decodes, and a decoded picture is the desktop:
right colours, right geometry, no shearing and no tearing. So the single-memory
plus video-meta description is correct for this layout, and neither fallback the
handoff note listed was needed.

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

### Restating the input size on the filter is free, and the memory feature is not

An earlier revision of this document said the opposite: that restating a size the
scaler is not changing defeats a pass-through and doubles the cost, from 9.2 to
18.6 milliseconds at full resolution. **That is wrong.** Measured on the real
pipeline, 400 access units per run, three runs of each variant, every run at the
refresh rate, at 3440x1440:

| filter caps | median | ninetieth |
|---|---|---|
| left open, `ANY` | 7.64, 7.64, 7.64 ms | 7.83, 7.80, 7.79 ms |
| `video/x-raw(memory:VAMemory)` with the same size | 7.64, 7.62, 7.57 ms | 7.82, 7.78, 7.78 ms |
| bare `video/x-raw` with the same size | 7.60, 7.64, 7.61 ms | 7.80, 7.81, 7.80 ms |

There is no difference at all, let alone a doubling. The filter is therefore set
unconditionally, which removes a branch and keeps one negotiated memory type for
the life of the pipeline instead of two.

The memory feature does decide what gets negotiated, though. Reading the caps
back off the pads proves it:

- filter left open: `video/x-raw(memory:DMABuf), format=DMA_DRM,
  drm-format=NV12:0x0200000010401b04`. The scaler hands the encoder a descriptor.
- `(memory:VAMemory)`: `video/x-raw(memory:VAMemory), format=NV12`. A VA surface.
- bare `video/x-raw`: `video/x-raw, format=NV12`. **System memory**, on every pad.

So caps with no feature field really do mean system memory, and both elements
honour that. It costs nothing measurable here because an integrated part shares
one physical memory with the processor, but it would on a discrete GPU, and it is
free to state. The filter carries `(memory:VAMemory)`.

### The whole chain, measured end to end

Time from handing a shared buffer to `appsrc` to holding the access unit, so
scaler and encoder together rather than the encoder alone. Captured at 3440x1440,
four interleaved passes of each size, every pass at about 72 frames per second:

| encode size | median | ninetieth | ninety-ninth | worst |
|---|---|---|---|---|
| 3440x1440, no scaling | 7.61 to 9.15 ms | 7.79 to 9.38 ms | 7.94 to 9.54 ms | 16.75 ms |
| 2580x1080 | 8.99 to 9.82 ms | 9.91 to 9.97 ms | 10.01 to 10.10 ms | 15.37 ms |
| 1720x720 | 5.04 to 5.11 ms | 5.22 to 5.25 ms | 5.36 to 5.43 ms | 11.33 ms |

The middle rung is the slowest, which is the interesting part. 1720x720 is
exactly half of 3440x1440 in both axes and costs 5.1 ms; 2580x1080 is
three quarters and costs 9.8 ms, more than not scaling at all. **The ladder
should be built from halvings**, because an arbitrary ratio makes the scaler
cost more than the encoder saves.

Encode latency also roughly doubles when frames arrive intermittently rather than
at the refresh rate: the same native size measures 7.6 ms at 72 frames per second
and 15.0 ms over a run averaging 34. The part downclocks between frames. Any
latency figure is only meaningful next to the rate it was taken at.

### Variable bitrate emits no filler, and the bitrate is not a ceiling

Filler measured 0.000 percent of the bitstream in every run at every size, which
confirms the switch away from constant bitrate.

The configured rate is a target and not a limit. Against 20 Mbit/s configured,
the achieved rate at 3440x1440 ranged from 20.7 to 35.8 Mbit/s depending on how
hard the picture was, and the lower rungs came in under it: 13.7 to 14.0 Mbit/s
at 2580x1080 and 7.0 to 8.7 at 1720x720. Congestion control must treat the
bitrate property as a request, not as a bound it can rely on.

### Three shared buffers is the measured minimum

Not a margin. With the ring cut to two, `publish` reaches a state where no buffer
is free to arm and none can be freed by displacing the pending frame, within a
second of starting, every time. With three it never happens under the encoder:
over about eleven thousand frames the starvation path, where nothing is free
until the pending frame is displaced, was taken 0 to 56 times per run and always
recovered on the same pass, so no vertical blank was missed.

Pushing a buffer and immediately pulling the sample does make the chain serial,
as was reasoned but not measured before. Pushes and pulled samples stayed one to
one across every run, and a decoded frame shows no tearing, so the return of the
pull really is a safe signal that the scaler has finished with the shared buffer.

### Live reconfiguration, measured on one pipeline

Seven changes on a pipeline that was never torn down, 300 access units collected
after each:

| change | applied in | took effect on | dropped |
|---|---|---|---|
| 3440x1440 to 2580x1080 | 0.052 ms | next unit | 0 |
| 2580x1080 to 1720x720 | 0.054 ms | second unit | 0 |
| back to 3440x1440 | 0.034 ms | next unit | 0 |
| bitrate doubled | 0.058 ms | next unit | 0 |
| declared frame rate halved | 0.063 ms | next unit | 0 |

Every one of them forces a key frame, resolution changes included, and the
encoder renegotiates its level in place: the streams report level 3.2 at
1720x720 and 5.0 at 2580x1080 and above. Each phase was written to its own file
and every one decodes through `h264parse ! avdec_h264` with no error, including
across the resolution change in the middle of it.

"Second unit" rather than "next" is the frame that was already inside the
pipeline when the property was set, not a delay in applying it.

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

Three runs, the third with the machine busy compiling, which is where the wider
figures come from.

| | |
|---|---|
| Delivered | 180 of 180, every run. 2400 datagrams, none evicted, none lost |
| Carried | 26.74 Mbit in 2.985 s, 8.96 Mbit/s |
| Capture to delivery | median 1.42, 1.45, 2.41 ms |
| | 99th percentile 2.56, 2.42, 7.85 ms |
| | worst 2.80, 3.51, 10.54 ms |
| Round trip | 193, 154, 814 µs, quinn's own path estimate |
| BBR pacing rate | 670, 721, 280 Mbit/s |
| Congestion events | 0 |

Capture to delivery is measured from the moment the frame was due to be sent, so
it holds the sender's own scheduling and none of the encoder. On an idle machine
the transport costs about one and a half milliseconds of the ten to twenty
millisecond budget. Under load it costs as much as ten, and every millisecond of
that is scheduling rather than network: the round trip moves with it.

The same link driven as hard as the gate allows, for 1.5 seconds: 1.1 to
3.3 Gbit/s carried, round trip 29 to 209 µs, BBR pacing rate 2.9 to 3.4 Gbit/s.
Of the datagrams that arrive, about 1% are lost, which is the kernel dropping UDP
at gigabit rates and not the transport evicting: an eviction would have ended the
connection instead.

That run also shows the frame identifier rule doing its work. Of 15,968 frames
offered, the gate refused 4616 whole, and the client counted 4704 frames missing:
the refusals, plus the handful the kernel dropped entirely. Every one of those
refusals is an encoded picture the encoder's reference chain has already moved
past, so the client is right to count them as loss, and the 4679 complete frames
it then threw away are the delta frames that referred back to them. On a link that
is not being deliberately overrun, the gate refuses nothing and none of this
happens.

The pacing rate is only ever `Some` under BBR. Cubic and NewReno report `None`, so
choosing BBR is the precondition for having a bandwidth estimate at all rather
than a preference between controllers.

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

### The Linux path, measured

Built and run. A test pattern encoded by `vah264enc` at 1920x1088 was decoded and
drawn in a Flutter window on Wayland, on both renderers. Numbers below are from a
release build.

The pipeline is
`appsrc(alignment=au) ! h264parse ! vah264dec ! video/x-raw(memory:DMABuf) !
glupload ! glcolorconvert ! appsink(memory:GLMemory, RGBA, texture-target=2D)`.
Three caps in it are load-bearing and each one turns a silent regression into a
negotiation failure: `memory:DMABuf` after the decoder, `texture-target=2D` at the
sink, and `format=RGBA`.

`glupload` chooses the `DirectDmabufExternal` method, which imports the decoder's
buffer as an `EGLImage` and gives back a `GL_TEXTURE_EXTERNAL_OES` texture.
`glcolorconvert` then converts to `GL_TEXTURE_2D` RGBA in one shader pass. That
pass cannot be removed: Flutter demands `GL_TEXTURE_2D` and `GL_RGBA8`, and the
decoder produces tiled NV12. It costs GPU time, not a copy.

- Decode to texture ready, at a 60 Hz feed: median **0.63 ms**, 95th percentile
  **0.85 ms**, worst **1.34 ms**.
- Texture ready to Flutter's rasteriser: nothing measurable. End to end is the
  decode figure plus the wait for the compositor's next frame, which every client
  pays. The lowest end-to-end figure seen was **1.87 ms**, the highest one frame
  interval more.
- Sustained throughput, fed as fast as the pipeline accepts: **430 frames a
  second** at 1920x1088, seven times what 60 Hz asks for.
- Steady state drops nothing. Frames replaced before Flutter asks for them fall
  to zero once the first second is past.

The CPU cost is what proves nothing is copied. Per frame at 1920x1088, all
threads counted:

| path | CPU per frame |
|---|---|
| decoder to shared buffer to texture, as above | **1.0 ms** |
| decoder to system memory, `videoconvert` on four threads to RGBA | 5.0 ms |
| the same, then uploaded to a texture, which is what a pixel buffer fallback costs | 5.6 to 11.7 ms |

So a CPU fallback costs five to twelve times as much CPU per frame and spends
three to eleven milliseconds of the latency budget. It is not an option at 60 Hz.

Impeller is already the default on this version: the Linux embedder sets
`enable_impeller = TRUE` when it builds its project, and the log says
`Using the Impeller rendering backend (OpenGLESSDF)` with no flag passed. Both
renderers draw the picture correctly, with correct colour. Skia was reached with
`FLUTTER_ENGINE_SWITCHES=1` and `FLUTTER_ENGINE_SWITCH_1=enable-impeller=false`,
which is the only way in: the runner's command line arguments go to the Dart
entry point, not to the engine.

`irondash_texture` 0.5.0, the published stable release, does carry the Linux
OpenGL payload. Its vtable still matches: `FlTexture` became a GObject interface
and `FlTextureGL` now derives from `GObject` directly, but `FlTextureGLClass` is
still `GObjectClass` followed by one `populate` pointer, so the layout the crate
declares is byte for byte the current one. The embedder still uses the texture's
address as its identifier, which is what the crate returns. No fork is needed.

Two things about the embedder that are not written down anywhere else:

- Without `alignment=au` on the `appsrc` caps the parser cannot tell where an
  access unit ends until the next one arrives, and holds every frame for exactly
  one frame period. That was 16 ms of the 17 ms first measured.
- A pipeline that cannot negotiate says so on the bus and nowhere else. The
  present path records the first such message and offers it, because a client
  showing no picture otherwise gives no reason for it.

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

## Pacing and loss on a real wireless link

Everything above about the transport was measured on loopback, where the round
trip is under a millisecond and nothing is ever dropped by a queue we do not
own. A Galaxy S25 over Wi-Fi 6 is a different link, and the difference is not a
matter of degree. In sixteen seconds of streaming to it: 859 packets in, 24
lost, 40 late; 63 frames in, 8 missing, 12 undecodable; round trip 13.91 ms; the
host gated 179 frames while sending 64.

This section is why. The probe is a real quinn host and a real quinn client with
the shipped transport configuration, joined by a userspace relay that gives the
path a chosen one-way delay, a chosen bottleneck rate with a finite drop-tail
queue, a chosen loss rate, and a chosen per-packet jitter. Nothing about quinn is
modelled: its congestion controller, its pacer, its datagram queue and the
shipped gate are all the real ones reacting to the shaped path. Frame sizes are
the ones measured on this machine, 130,427 bytes for a 1080p key frame and
16,673 for a P frame. The client asks for a key frame over the real control
stream the moment reassembly says it cannot go on, and the host answers under the
shipped 250 ms budget, because a run without that measures nothing but how long
the first break lasts.

### The air hop, measured

1200 pings of 1400 bytes at 60 Hz from this laptop to its access point, one air
hop of the two the phone is behind:

| | |
|---|---|
| round trip | min 2.12, median 3.26, ninetieth 6.48, ninety-ninth 141, worst 243 ms |
| delay above the fastest probe | median 1.14, ninetieth 4.36, ninety-fifth 7.00 ms |
| probes more than 2 ms above the floor | **22.92%** |
| probes more than 5 ms above the floor | 8.33% |
| probes more than 10 ms above the floor | 3.42% |
| probes more than 50 ms above the floor | 2.25% |

Nothing was lost. The medium does not drop packets here, it delays them, and it
delays roughly a quarter of them by more than two milliseconds. Hold that figure
against the receiver's two millisecond reorder window; it comes back below.

The 13.91 ms round trip to the phone is mostly not this. This hop is 3.26 ms at
the median, so about ten milliseconds of the phone's round trip is the second
air hop, which is the phone's own radio waking up. **Reasoned, not measured:**
that is what Wi-Fi power save looks like, and it means the round trip to a phone
is not a queueing delay we can shorten by sending less.

### quinn paces, and its pacer does nothing for us

Settled by reading `quinn-proto` 0.11.17 and then measured.

The pacer is checked in `Connection::poll_transmit` before every new UDP
datagram, whatever the frames inside it are, so it applies to application
datagrams exactly as it applies to stream data. That much is right.

What it applies is `Pacer`, a token bucket whose capacity is
`window * 2 ms / smoothed_rtt`, clamped to between ten and 256 MTUs, refilling
at `1.25 * window / smoothed_rtt`. Two things follow. The rate is derived from
the congestion window, not from BBR's own pacing rate: `calculate_pacing_rate`
computes one and `Pacer` never reads it. And with BBR's steady-state congestion
window gain of 2.0, the effective pacing rate is about **2.5 times the
bandwidth estimate**, with a two millisecond burst allowance on top.

Measured, on a path with no bottleneck and a 13.91 ms round trip: the 92
datagrams of a 1080p key frame arrive at the client spread over **0.08 ms** at
the median. A twelve datagram P frame spreads over 0.02 ms. quinn puts 130 kB on
the wire in eighty microseconds, which is about thirteen gigabits a second.

So the pacer exists, it applies to datagrams, and at these rates it never
engages. **A pacer of our own is needed, and quinn's is not a substitute.**

### quinn's bandwidth estimate is not a bandwidth estimate

This contradicts a claim made earlier in this document, and it is the most
important thing in this section. That claim was:

> The congestion controller applies to datagrams. [...] So the bandwidth estimate
> comes free from the transport, and a delay-based estimator of our own is not
> needed on a local network.

**The first sentence is right and the second is wrong.** The estimate does not
come free. On a link with headroom it does not come at all.

`BandwidthEstimation::on_ack` updates its filter only when
`!app_limited && self.max_filter.get() < bandwidth`, and `Connection::app_limited`
is set at the end of every `poll_transmit` to `buf.is_empty() && !congestion_blocked`.
It is therefore a statement about whether the *last* `poll_transmit` had anything
to send, not about whether the connection was application limited when the packet
being acknowledged went out. A 60 Hz video sender fills quinn's queue in a
hundred microseconds and then has nothing for sixteen milliseconds, so by the
time the acknowledgements come back a round trip later, `app_limited` is true and
no bandwidth sample is ever taken. The guard `max_filter.get() < bandwidth` also
means the filter is only ever raised, so once a sample is recorded it never
decays.

Measured, sampling `Connection::congestion_state().metrics().pacing_rate` every
50 ms. A 9 Mbit/s stream on a 100 Mbit/s path, then the path drops to 6 Mbit/s at
ten seconds:

| t | reported pacing rate |
|---|---|
| 0 to 4 s, path 100 Mbit/s | **0.00 Mbit/s** |
| 5 to 10 s, path 100 Mbit/s | 0.76 to 0.95 Mbit/s |
| 11 to 30 s, path 6 Mbit/s | 7.31 Mbit/s, with excursions to 12.19 |

On a healthy hundred megabit path the transport reports zero, or under one
megabit. On a six megabit path it reports seven to twelve. The sign of the error
is the dangerous one: it reads far too low exactly when there is headroom, and
too high when there is none. In a separate twenty second run at 9.2 Mbit/s on a
100 Mbit/s path with a deep queue, every one of four hundred samples was
**0.0 Mbit/s**.

`Ladder::observe` is fed `pacing_rate * 85 / 100` through `connection_target`. A
target of zero drives the bits-per-pixel ratio to zero, which is below `STARVED`,
which steps the ladder down every two seconds until it reaches its bottom rung.
The one thing keeping this from being visible on the real link is that the real
link congests, which is what makes the estimate exist at all.

This does not change the conclusion that the congestion controller applies to
datagrams. It changes the conclusion that we can read a rate off it. **An
estimator of our own is needed, and the reason is not that quinn's is
imprecise. It is that quinn's is absent whenever the link is fine.**

### Why a burst fails, and it is the buffer and not the rate

A burst does not fail because the link is too slow. It fails because the
bottleneck's buffer is smaller than the frame.

Measured, 100 Mbit/s path, 13.91 ms round trip, no loss and no jitter
configured, twenty seconds at 60 Hz, key frame every two seconds, bursting as the
transport does today. The only variable is how many bytes the bottleneck may hold
before it drops the tail:

| bottleneck buffer | frames delivered | packets dropped by the queue |
|---|---|---|
| 16 kB | 0.1% | 8067 |
| 32 kB | 12.0% | 5549 |
| 64 kB | 12.1% | 3628 |
| 96 kB | 18.8% | 1458 |
| **128 kB** | **97.4%** | 1 |
| 192 kB | 97.9% | 0 |

The step is at one key frame. 130,427 bytes of key frame offered in eighty
microseconds either fits the buffer at the far end or does not, and at a hundred
megabits that buffer drains 32 kB in 2.6 ms while we fill it in a tenth of a
millisecond. The link's rate never enters into it.

We do not choose that buffer, we cannot see it, and it is a per-station queue
inside an access point. **So the transport must not depend on its depth.**

The same thing from the other side, bursting into a 32 kB buffer with the key
frame capped at various sizes:

| key frame | frames delivered | peak backlog |
|---|---|---|
| 130,427 B, as measured | 0% to 68% depending on the seed | 32,739 B, full |
| 65,000 B | 0% | 32,752 B, full |
| 40,000 B | 6.2% | 32,734 B, full |
| **25,000 B** | **100%**, worst latency 13.47 ms | 24,637 B |

The spread on the first row is not noise to be averaged away. Bursting into a
buffer that cannot hold the frame is bistable: either the stream limps along, or
one destroyed key frame provokes a request for another one, which is another
burst into the same full buffer, and the session never recovers. Three seeds of
the same 32 kB configuration delivered 51.7%, 7.3% and 0.0% of frames.

### Pacing works, and the rate to pace at is the path's

Pacing over a fixed slice of the frame interval, which is the obvious design, is
the wrong one. Measured on an unbottlenecked path: spreading every frame over
0.9 of a frame interval delivers 100% of frames and moves the median latency from
**9.21 ms to 23.64 ms**. A frame is worth nothing until its last datagram, so
spreading it over fourteen milliseconds costs fourteen milliseconds, on a budget
of ten to twenty.

Pacing at a byte rate is right, and the rate is roughly the path's own.
Measured, 50 Mbit/s path, 32 kB buffer, twenty seconds:

| how the frame is offered | delivered | packets lost | median latency | worst |
|---|---|---|---|---|
| burst, as today | 1.3% | 5446 | 12.20 ms | 33.77 ms |
| paced at 10 Mbit/s | 0% | 0 | key frames took 104 ms and timed out |
| paced at 20 Mbit/s | 0% | 0 | key frames took 52 ms and timed out |
| **paced at 40 Mbit/s** | **100%** | **0** | **13.44 ms** | 37.92 ms |
| paced at 80 Mbit/s | 0% | 1713 | overran the buffer again |

And on a 100 Mbit/s path with the same 32 kB buffer, three seeds each:

| | delivered | packets lost | median | ninetieth | ninety-ninth | worst |
|---|---|---|---|---|---|---|
| burst | 51.7%, 7.3%, 0.0% | 2897, 5887, 6328 | 10.6 to 11.3 ms | 11.1 to 14.0 | 15.3 to 42.6 | 46 to 70 ms |
| **paced at 80 Mbit/s** | **100%, 100%, 100%** | **0, 0, 0** | 11.6 to 11.8 ms | 12.4 to 12.7 | 13.4 to 14.3 | 24.6 to 25.0 ms |

Pacing at eighty percent of the path rate costs about **one millisecond of median
latency** and takes the stream from between zero and half of its frames to all of
them. It also improves every percentile above the median, because a burst that
overfills a queue delays everything behind it: the ninety-ninth percentile falls
from as much as 42.6 ms to 14.3 ms and the worst case from 70 ms to 25 ms.

The window is narrow on both sides, and that is the design problem. Too fast and
the buffer overruns again. Too slow and a key frame takes longer than the
receiver will wait for it. At 25 Mbit/s the same sweep put the sweet spot at 20
to 25 Mbit/s, delivering 98.2% and 99.1%, so the rule "pace at about the path
rate" holds across a factor of four in path rate. **Measured.**

Pacing at quinn's own reported rate does not work, for the reason in the section
above: measured, it collapsed the stream to 45.8% of frames on an unbottlenecked
path, because the estimate falls, which makes the pacer slower, which makes the
sender more application limited, which lowers the estimate further.

### The key frame is the whole problem

At 8 Mbit/s a 130 kB key frame is 130 ms of the stream's average budget delivered
in one sixteen millisecond slot. Every failure above is a consequence of that
one ratio. Pacing moves the queue from the access point to us, where we can see
it, but it cannot make the bytes smaller: paced at 40 Mbit/s a key frame's
datagrams still span 26.6 ms at the client, and at 25 Mbit/s they span 43.8 ms.

Three ways out, in the order they should be taken:

1. **Cap the key frame.** Measured above: at 25,000 bytes a key frame is
   invisible even to a burst into a 32 kB buffer. The encoder has no direct
   control for this, but the coded picture buffer does have a floor of half the
   bitrate, and the bitrate itself is a per-frame budget divided by the declared
   frame rate. This is the cheapest lever and it is not yet pulled.
2. **Spread it over more than one frame interval.** Measured to work only if the
   receiver's deadline is raised to match. Measured: on a 25 Mbit/s path, pacing
   the key frame over 70 ms delivered 97.2% of frames against a 120 ms frame
   deadline. Against the shipped 50 ms deadline the same shape fails outright: on
   a 50 Mbit/s path a key frame paced over 52 ms delivered **0%**, because every
   key frame timed out one or two milliseconds before its last datagram arrived.
   The frames that fall inside the key frame's span are never encoded, so they
   cost no frame identifier and no reference chain.
3. **Intra-refresh**, which removes key frames entirely. Still not exposed by
   GStreamer on either GPU here. VA-API's `VAEncMiscParameterTypeRIR` is the
   parameter and `cros-libva` is the road to it. **Not measured. Nothing here
   changes the earlier judgement that this is a large undertaking**, but every
   measurement in this section raises what it is worth.

**Reasoned:** the ordering matters because 1 and 2 are settings and 3 is a new
encoder backend.

### Loss resilience, re-derived for this link

The earlier finding chose recovery on demand over parity, and its reasoning was:

> On a local network the round trip is under a millisecond, so parity on every
> frame would be paying continuously for something that costs one round trip and
> one IDR when it happens.

**That reasoning is sound and its premise is gone.** On this link the round trip
is 13.91 ms and a frame period at 60 Hz is 16.67 ms.

The number that decides everything is the survival probability of one frame.
Closed form, independent loss at the measured 2.79%:

| | delta, 12 datagrams | key, 92 datagrams |
|---|---|---|
| no parity | 0.712 | **0.074** |
| 20% parity | 0.9994 | 1.0000 |

An unprotected key frame arrives intact **seven times in a hundred**. Everything
the client sees follows from that: the chain breaks, the client asks for another
key frame, and that one has a seven percent chance too.

Monte Carlo over a 60 fps stream, twelve runs of sixty seconds each, with the
chain rule applied (one lost frame makes every later frame undecodable until a
key frame lands) and the shipped 250 ms key frame budget. `hold` is how long the
client keeps an incomplete frame open:

At the measured 2.79% independent loss, hold of one frame period:

| scheme | frames shown | freezes | median freeze | bandwidth |
|---|---|---|---|---|
| none, as today | 1.5% | 0.27/s | 2533 ms | 11.89 Mbit/s |
| retransmit only | 5.2% | 0.37/s | 2017 ms | 12.00 Mbit/s |
| 10% parity everywhere | 98.3% | 0.38/s | 33 ms | 10.37 Mbit/s |
| **20% parity everywhere** | **99.9%** | 0.03/s | 33 ms | 10.73 Mbit/s |
| 20% on delta, none on key | 88.4% | 0.04/s | 2300 ms | 11.07 Mbit/s |
| 20% on delta, 5% on key | 99.8% | 0.03/s | 33 ms | 10.72 Mbit/s |

The same, with the hold raised to two frame periods:

| scheme | frames shown | median freeze | bandwidth |
|---|---|---|---|
| none, as today | 1.5% | 2533 ms | 11.89 Mbit/s |
| **retransmit only** | **99.3%** | 850 ms | **8.81 Mbit/s** |
| 20% parity everywhere | 99.9% | 33 ms | 10.73 Mbit/s |
| 20% parity and retransmit | 100.0% | 0 ms | 10.69 Mbit/s |

Four things come out of this.

**Retransmission does fit, but only just, and only if the client waits two frame
periods.** The repair takes the round trip plus however long it takes to notice.
A gap is seen when the next datagram after the hole arrives, which inside a paced
frame is a fraction of a millisecond, but a hole at the tail of a frame is proven
only by the next frame's first datagram, a whole frame period later. So the
repair lands between 14 and 31 ms after the loss. At a hold of one frame period
it shows 5.2% of frames; at two it shows 99.3%. **The earlier decision was not
wrong about retransmission being cheap. It was wrong about it being free of
latency.**

**Parity is the only scheme that needs no extra hold**, because the repair
travels with the frame. Its freezes, when it has any, are one frame long rather
than a second long.

**Giving key frames no parity is wrong for us, and it is wrong for a reason
specific to our frame sizes.** Sunshine can afford it; our key frame is 92
datagrams against a delta frame's 12, so it is the frame most exposed to loss and
not the least. Measured: 88.4% shown against 99.9%, with a median freeze of 2.3
seconds. Five percent parity on a key frame is five datagrams and takes it to
99.8%. **The rule is not "protect key frames more" or "less", it is that the
parity must be sized against the frame's datagram count, which is what a
percentage already does.**

**Under bursty loss both schemes degrade and neither is sufficient alone.** Same
long-run rate of 2.79% but arriving in runs of three: today's scheme shows 21.2%,
retransmission alone 75.2%, 20% parity 81.9%, and the two together 97.2% at 11.38
Mbit/s. This is the case that argues for having both.

The bandwidth arithmetic is worth stating plainly, because "20% FEC" sounds
expensive and is not. At 8.5 Mbit/s of picture, 20% parity is 1.7 Mbit/s. One
recovery key frame is 130 kB, which is 1.04 Mbit. Today's scheme spends 3.4 key
frames a second at this loss rate, which is 3.7 Mbit/s of key frames alone. **The
scheme with no parity is the more expensive one.** Measured: 11.89 Mbit/s for
today against 10.73 Mbit/s for 20% parity everywhere, and the second one shows
99.9% of frames against 1.5%.

### Slices buy nothing until the receiver changes

`num-slices = 1` today, and the earlier measurement that rejected more slices
measured latency on a local network: 7.30 ms mean at one slice against 8.81 ms
mean and 23.74 ms worst at four. That measurement stands.

What it did not consider is that **slices buy nothing at all with the receiver we
have.** `Reassembler` delivers a frame only when every slice of it is complete:
`Partial::complete` requires `slices.len() == last + 1` and every slice full. A
frame missing one datagram of one slice is dropped whole, exactly as a
single-slice frame is. Read from the source, and it is not a subtlety, it is the
first condition in the function.

So slicing is a change to three things at once: the encoder's `num-slices`, the
receiver's willingness to hand a partial access unit to the decoder, and the
decoder's concealment. Only the first is a setting.

What the third would buy, if the first two were done. Measured over the frames
actually damaged in the runs above, by asking of each damaged frame's arrival
record how many bands would have survived had the frame been cut into that many:

| the damaged frames of | intact at 2 slices | at 4 | at 8 | at 16 |
|---|---|---|---|---|
| a 100 Mbit/s path with a 128 kB buffer, 13 frames | 50% | 75% | 88% | 94% |
| a 50 Mbit/s path with a 32 kB buffer, 24 frames | 15% | 44% | 51% | 63% |
| a 25 Mbit/s path with a 32 kB buffer, 50 frames | 2% | 4% | 16% | 23% |

The rows are ordered by how hard the bottleneck was pressed, and the trend is the
point. When a frame loses the odd scattered datagram, slices do what the
architecture expected: cutting into four leaves three quarters of the picture.
When the loss comes from a buffer overrunning, it does not scatter. It takes the
tail of the frame, contiguously, because that is what a drop-tail queue does, and
two slices then save two percent of the picture rather than half of it. Slices
are worth something against **independent** loss and nearly nothing against
**queue** loss, and queue loss is what a burst produces.

**Recommendation, firm: do not add slices.** They cost 1.5 ms of encoder latency,
they require a partial-frame path through the receiver and the decoder, and
against the loss this link actually produces they recover a fifth of a picture at
best. Parity over the whole frame recovers all of it for less. `slice_id` 255 and
the spare flag bits stay reserved, and the field stays in the header, because
parity is what they were reserved for.

### The two millisecond reorder window is the single worst constant

`REORDER_WINDOW` is 2 ms, and its documentation says why: "a reordered packet on
a link whose round trip is under a millisecond arrives immediately". That is
true, and this is not that link. Measured above: **22.92% of packets on one air
hop arrive more than 2 ms behind the fastest one.**

Measured in the probe, on a path with no loss whatsoever, paced so that no queue
ever overruns, with three percent of packets held back by up to 10 ms as a link
layer retry does. Twenty seconds, 1200 slots:

| reorder window | frames delivered | frames dropped | late packets | of those, ones that would have completed the frame |
|---|---|---|---|---|
| **2 ms, as shipped** | **1.8%** | 384 | 5177 | 384 |
| 5 ms | 8.3% | 241 | 2906 | 241 |
| 10 ms | 46.8% | 50 | 350 | 50 |
| **15 ms** | **94.6%** | 5 | 4 | 4 |
| 25 ms | 94.9% | 4 | 4 | 4 |

The receiver used for that sweep is the shipped `Reassembler` with its two
constants lifted into parameters, and it was checked against the shipped one
rather than assumed equal to it: over 3913 datagrams carrying 300 frames, with
2% of datagrams dropped and 5% held back by up to 10 ms, the two agreed on every
frame completion and on every one of the seven counters. That run is worth
quoting on its own, because it is the shipped receiver on a link far gentler than
the measured one: **0 frames of 300 delivered**, 164 dropped and 136 undecodable.

Nothing was lost on the jitter-only link above. Every one of those 384 dropped
frames was thrown away while the datagram that would have completed it was in the
air. That is the
40 late packets against 24 lost in the original measurement, and it is why the
late count is larger than the loss count.

The window costs nothing to widen. Median latency across the whole sweep is
11.85 to 12.13 ms, flat. The ninetieth percentile **improves**, from 24.25 ms at
2 ms to 18.80 ms at 15 ms, because a frame dropped at 2 ms is not a frame that
arrived sooner, it is a frame replaced by a key frame request and a break in the
chain.

The reason widening is free is worth stating, because it is not obvious. The
window only ever applies to a frame that is already known to be damaged. An
undamaged frame is delivered the instant its last datagram arrives and never
looks at the window at all. So the window trades nothing against the good case;
it trades a damaged frame's delay against not having that frame.

The ceiling on it is a frame period. A datagram that arrives after the next frame
is complete cannot be used: the receiver has already moved its floor past it.

**Reasoned, not measured:** the window should be derived rather than fixed,
because 15 ms is right for 60 Hz and wrong for 120. One frame period is the
principled figure, and the client knows the frame period from `StreamConfig`.

A second, smaller thing in the same function. When a frame completes,
`abandon_below` decides every frame under it on the spot, with no window at all,
even a frame that is one datagram short and inside its reorder window. Measured
as a variant that lets those frames keep their window: it helps, 1.8% to 4.2% at
a 2 ms window, and it is dominated by simply widening the window, 94.6%. **Not
worth the complexity on its own. Widen the window instead.**

### The gate closes on the wrong question

Not the subject of this research, but it fell out of it and it explains the
"gated 179 while sending 64" line.

The admission gate asks `can_send_bytes(key_frame_estimate())` before every
frame, delta frames included. That is room for 130 kB in a 204 kB buffer, so the
gate shuts as soon as 74 kB is queued, which is about five P frames of backlog.
On a link that cannot drain a key frame in one frame interval, that is most of
the time. The gate is right that a key frame must fit whole; it is asking that
question about frames that are not key frames.

**Reasoned:** the gate should ask for the frame it is about to encode, and ask
for a key frame's room only when it is about to ask for a key frame.

### What WebRTC does, for comparison

Read from the libwebrtc source at tip and cross-checked against seven release
branches, by a second researcher. **Read, not run.** It is worth recording
because two of the recommendations below match it and one deliberately does not.

**It paces at a byte rate with a burst allowance, not over a frame interval.**
`PacingController` grants a burst of `min(kDefaultBurstInterval, kMaxBurstSize /
rate)`, where `kDefaultBurstInterval` is 40 ms
(`modules/pacing/pacing_controller.h`, renamed `PacerConfig::kDefaultTimeInterval`
at tip) and `kMaxBurstSize` is 63,000 bytes. The crossover is a pacing rate of
12.6 Mbit/s: below it the time bound binds, above it the byte bound does. At a
target of 10 Mbit/s with the usual pacing factor of 2.5, a 130 kB key frame takes
**41.6 ms** to leave, about two and a half frame intervals. So WebRTC does not
try to fit a key frame into one frame interval, and it does not burst it either.
That is the shape recommendation 1 asks for.

Its `queue_time_limit` of two seconds, the rule that raises the pacing rate to
drain a long queue, is inert at our scale: triggering it at 25 Mbit/s needs 6.25
MB queued and our key frame is 130 kB.

**It caps key frame size for VP8 and not for H.264.**
`LibvpxVp8Encoder::MaxIntraTarget` feeds `VP8E_SET_MAX_INTRA_BITRATE_PCT`, which
at 60 fps works out to 1800% of the per-frame budget. `H264EncoderImpl` sets
`iMaxBitrate = UNSPECIFIED_BIT_RATE` and `uiMaxNalSize = 0`, so H.264 key frames
are uncapped and only the frame dropper and the pacer stand behind them. We use
H.264, so we are on the uncapped side of that, which is what recommendation 3 is
about.

**It protects key frames harder, not less.** The parity count is
`(num_media_packets * protection_factor + 128) >> 8`
(`modules/video_coding/utility/...` and `forward_error_correction.cc`), and the
key frame factor is boosted by `BoostCodeRateKey = max(2, packetFrameKey /
packetFrameDelta)` with `_scaleProtKey = 2.0` in `media_opt_util.cc`. At our
92-against-12 datagram ratio that boost is 7, which drives the key frame
protection factor to its clamp: 40 to 50 percent on key frames against 20 to 32
percent on delta frames. Our own Monte Carlo reaches the same conclusion from the
survival probabilities, and it is the opposite of giving key frames no parity.

It turns FEC off above a round trip of 200 ms (`kMaxRttTurnOffFec`, function
local in `BitRateTooLowForFec`), which is far above ours, so on this link WebRTC
would carry both parity and retransmission.

Two constants worth knowing about because of how far they are from ours: its
NACK list tolerates `kMaxReorderedPackets = 128` and runs on a
`kUpdateInterval` of 20 ms (`modules/video_coding/nack_requester.{h,cc}`), and
its retransmission window is `kRetransmitWindowSizeMs = 500`. Our receiver gives
up on a frame 2 ms after it first suspects a gap.

### Recommendations

1. **Pace, in the transport, at a byte rate.** Spread a frame's datagrams evenly
   over `bytes * 8 / rate`, where `rate` is a target the session owns. Do not
   pace over a fixed slice of the frame interval; that costs the whole slice.
   Measured cost at the right rate: about one millisecond of median latency.
   Measured benefit on a 32 kB bottleneck: from between 0% and 52% of frames to
   100%.
2. **Do not take that rate from quinn.** `pacing_rate` is zero on a healthy link
   and never falls on a bad one. Until an estimator exists, pace at a multiple of
   the encoder's own configured bitrate, which is a number the session already
   sets, with a floor that keeps a key frame inside the receiver's deadline.
3. **Cap the key frame before anything else.** It is a setting, and at 25 kB the
   whole problem disappears even without pacing.
4. **Carry parity, sized as a percentage of the frame's datagrams, on every
   frame including key frames.** 20% at the measured loss takes the stream from
   1.5% to 99.9% of frames shown, and it does so for **less** total bandwidth
   than today's key-frame-on-demand recovery, because a key frame costs eight
   frames and parity costs a fifth of one. Keep recovery on demand as the
   backstop for the case parity cannot cover.
5. **Widen the reorder window to one frame period**, and derive it from the frame
   rate in `StreamConfig`. This is a one line change and it is worth 384 frames
   in 1200 on a link with no loss at all.
6. **Do not add slices.**

### What was not measured

The phone was not attached during this work, so nothing here was taken against
the real Android client. The link characterisation is the laptop's own air hop,
which is one of the two the phone sits behind.

An attempt to show a burst disturbing the real radio more than a paced load did
not separate the two. The medium's own variance is far larger than the effect:
ping to the access point under no load at all measured a worst case of 234 ms
and a mean deviation of 22 ms, which swamps anything an 8.5 Mbit/s stream does to
it. The claim that pacing helps rests on the shaped-link measurements and on the
buffer-depth mechanism, not on a measurement of this access point.
