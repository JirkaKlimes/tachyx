# Platform findings

What was measured on the target machine, and which assumptions in
[`architecture.md`](architecture.md) it disproved.

Target: NixOS 26.05, Hyprland 0.56.2, AMD Radeon 780M at `/dev/dri/renderD128`
(Mesa 26.1.2, VA-API 1.24, libva 2.24.0), NVIDIA RTX 4060 Laptop, GStreamer
1.28.6. Outputs: eDP-1 at 2880x1800 and 120 Hz, DP-2 at 3440x1440 and
74.983 Hz. A figure not marked as re-measured comes from the earlier shell
(VA-API 1.23, GStreamer 1.26.11).

An unmarked statement was executed. **Read** means taken from source or
documentation, not run. **Inferred** means reasoned, not measured.

## Assumptions that were disproven

The architecture document keeps its original design. These lines of it are
wrong for this target.

### Damage regions do not exist in practice

The architecture says capture is "DMA-BUF frames + damage regions", with "damage
awareness: static screen implies near-zero bitrate". Hyprland 0.56.2 reports
full-frame damage on every frame, on `ext-image-copy-capture-v1` and on
`zwlr-screencopy-v1`: 400 frames of 400, even with a one-pixel `damage_buffer`
hint.

Idle suppression comes from capture instead. `capture()` blocks until the screen
changes. On an empty workspace: 37 frames in 30 seconds (1.23 per second, about
one second apart), and 10 ms of process CPU time (0.03% of one core). This
replaces a 91 ms median between idle frames, taken while something small still
moved. The host reads no damage events. Capture sends a full-frame
`damage_buffer` hint (`crates/tachyx-media/src/capture.rs`).

### Portal RemoteDesktop does not exist on this compositor

The architecture makes "portal RemoteDesktop (libei)" the default input path
and the wlroots protocols the fallback. `xdg-desktop-portal-hyprland` 1.4.1
declares Screenshot, ScreenCast, GlobalShortcuts and InputCapture.
`org.freedesktop.portal.RemoteDesktop` is absent from the bus. The wlroots
protocols are the only path.

### Intra-refresh and long-term reference frames are unavailable

The architecture specifies "intra-refresh (rolling I-columns, no IDR spikes) +
long-term reference frames", where "on loss, client requests a P-frame off the
last acked LTR". Neither `vah264enc` nor `nvh264enc` has an intra-refresh or LTR
property, so no GStreamer path reaches either on either GPU. Every recovery is a
full IDR, and the bandwidth headroom must budget for IDR spikes. VA-API has an
intra-refresh parameter that the element does not expose, reachable through
`cros-libva`. That is a large undertaking and is not started.

### The portal capture path is not a latency improvement

The architecture makes `xdg-desktop-portal` ScreenCast the default and the direct
wlroots protocol the second backend. On Hyprland the portal is strictly worse.
`xdg-desktop-portal-hyprland` is itself a `zwlr-screencopy` client that
republishes frames into PipeWire. It adds D-Bus negotiation, a process and at
least one buffer handoff per frame. It also loses the ext protocol's render-node
identification, its modifier list and its separate cursor metadata channel.

## Capture

`ext-image-copy-capture-v1` with `ext-output-image-capture-source-v1`, DMA-BUF,
no portal. Hyprland 0.56.2 also advertises `zwlr-screencopy-v1` version 3. The
tree has no screencopy fallback.

Re-measured with `tachyx-media`, not a probe. DP-2 under continuous motion: 600
frames in 8.000 seconds, 75.0 per second, refresh-locked. The wait between
frames is 13.33 ms median, 13.62 ms p90, 15.10 ms worst. Nothing dropped or
failed.

`presentation_time` is `CLOCK_MONOTONIC`. It lands 0.04 ms in the past at the
median and 0.09 ms at worst (an earlier figure was 1 to 4 ms). Hyprland stamps
the copy, not the scan-out, so it does not tell how long the picture has been
on the glass.

### eDP-1 could not be made to present under motion

The earlier 123.6 frames per second on eDP-1 did not reproduce. With the panel
connected and enabled, lid open, DPMS on, its own workspace and a full-screen
`videotestsrc` window at 144 frames per second, capture of eDP-1 yields 2.7
frames per second, about one second apart. The same binary on DP-2 at the same
moment yields 75.0. Hyprland does not repaint that output. VRR is on for it,
`solitary` and direct scan-out are reported blocked, and none of these explains
it. The refresh lock is measured on DP-2 only. Capture at 120 Hz is unknown, so
do not rely on the 8.3 ms figure.

### The cursor session describes its buffer only once the cursor moves

The cursor is excluded from the frame unless requested. A separate cursor
session supplies position, hotspot and a 48x48 ARGB image. That is the
client-side cursor the architecture asks for, at no cost.

`ext_image_copy_capture_cursor_session_v1.get_capture_session` sends `enter` and
`position` at once and then nothing. The constraints (`shm_format`,
`dmabuf_format`, `dmabuf_device`, `buffer_size`, `done`) arrive with the first
cursor update. On a still pointer they never arrive. One probe binary against
one compositor: `done` within 0.2 ms in five runs in a row with the mouse in use,
and never a few minutes later with the mouse untouched. An earlier measurement
with a freshly moved mouse hid this. `hotspot` is delayed the same way, which
the protocol allows: "compositors may delay this event until the client
captures a new frame".

So a bounded wait for `done` works only if the daemon starts while the mouse
moves. The session arms nothing and builds the shared buffer whenever the
constraints arrive. That also covers re-sent constraints for a resized cursor,
which the protocol allows at any time.

Direct capture needs no user consent on Hyprland. Any process that reaches the
Wayland socket can record the screen. This is a compositor property, for the
security notes.

### Traps

- Resolve the render node from the session's `dmabuf_device` by matching device
  numbers across `/dev/dri`. Here `card0` is the NVIDIA device and `renderD128`
  the AMD one: the indices are crossed. Allocation on the NVIDIA node fails with
  `EINVAL`.
- Remove `DRM_FORMAT_MOD_INVALID` from the advertised modifier list before
  allocation.
- Under an AMD compression modifier a single-plane `XR24` buffer can report extra
  planes for compression metadata. Add every plane to the buffer parameters.
- The modifier this pair negotiates, `0x0200000010401b04`, has delta colour
  compression off (bit 13 clear). The allocator reports one plane: offset 0,
  stride 11776 on eDP-1 and 13824 on DP-2. This replaces an earlier "three
  planes" with no modifier named. The plane loop stays, because the count comes
  from the allocator.
- A session permits one capture in flight. Concurrent sessions on one output
  work.

### The compositor's buffer imports into the encoder

A GBM buffer object on the session's render node, filled by the compositor,
goes to GStreamer as one memory with a `GstVideoMeta` for the plane offsets and
strides. It imports into `vapostproc` and reaches `vah264enc`. The output decodes
to the desktop with correct colours and geometry, no shearing and no tearing.
Neither fallback from the handoff note was needed.

## Encode

`vah264enc` on the AMD integrated GPU. Encoder latency, sink pad to source pad:
3.8 ms at 720p, 7.3 ms at 1080p (7.4 ms p99), 11.1 ms at 1440p.

NVENC works on this machine and has richer latency controls. To feed it, frames
must cross PCI Express from the integrated GPU, which is a copy and wakes a
power-hungry device on a laptop. No NVENC backend exists: `ENCODERS` in
`crates/tachyx-media/src/encode.rs` lists VA-API elements only.
[`assumptions.md`](assumptions.md) lists what a second backend needs.
`vah264lpenc` does not exist here, because the low-power entrypoint is an Intel
feature.

### Rate control constraints

- A bitrate change forces an IDR. The property setter marks the encoder for
  reconfiguration, which resets its state. So bitrate updates are rate-limited
  and applied only on a significant change, never per frame.
- More slices raise latency: 7.30 ms mean at one slice, 8.81 ms mean and
  23.74 ms maximum at four. The hardware encodes the whole frame either way.
  Slices serve NAL unit size and loss resilience, never latency.
- The coded picture buffer cannot go below 500 ms (next section).

### The coded picture buffer cannot be made smaller than half a second

`bitrate=8000` and `target-percentage=100`, so the peak is 8 Mbit/s. From
`vah264enc`'s own log:

| `cpb-size` asked for | what the encoder used |
|---|---|
| 4000 kb | 4,000,000 bits, which is 500 ms |
| 3999 kb | "Too small cpb_size: 3999", "Adjust cpb_size to: 16000", which is 2000 ms |
| 133 kb, one frame at 60 Hz | the same 16000, which is 2000 ms |

`vaav1enc` refuses the same way, in `_av1_calculate_bitrate_hrd`. The floor is
exactly half the peak bitrate: the 500 ms floor. Below it is a cliff, not a
clamp. One kilobit under, the buffer becomes **four times** the request
(2000 ms, also the default). So `cpb-size` is set on every reconfiguration.
**A one-frame buffer, which is what Sunshine sizes its VBV to, is unreachable
through this element at any operating point.** At 8 Mbit/s and 60 Hz one frame
is 133 kb and the floor is 4000 kb, thirty times larger.

The element derives the peak as `bitrate * 100 / target-percentage`. The encoder
sets 100 (`configure_encoder` in `crates/tachyx-media/src/encode.rs`). At 66 the
log read "Too small cpb_size: 4000, adjust to 24242", so a 500 ms request became
two seconds.

`cros-libva` reaches the buffer through `VAEncMiscParameterTypeHRD` and intra
refresh by the same route. The buffer is what makes a key frame 130 kB.

### Live reconfiguration

Seven changes on one pipeline, never torn down, 300 access units collected after
each:

| change | applied in | took effect on | dropped |
|---|---|---|---|
| 3440x1440 to 2580x1080 | 0.052 ms | next unit | 0 |
| 2580x1080 to 1720x720 | 0.054 ms | second unit | 0 |
| back to 3440x1440 | 0.034 ms | next unit | 0 |
| bitrate doubled | 0.058 ms | next unit | 0 |
| declared frame rate halved | 0.063 ms | next unit | 0 |

Every change forces a key frame, resolution changes included. The encoder
renegotiates its level in place: 3.2 at 1720x720, 5.0 at 2580x1080 and above.
Every phase decodes through `h264parse ! avdec_h264` with no error, across the
resolution changes. "Second unit" is the frame already inside the pipeline, not
a delay. A first test of the resolution path applied in 0.03 ms.

A resolution change sets caps on a filter between the scaler and the encoder.
The source keeps its caps at the monitor's size. This replaces a claim that a
downstream filter stalls the pipeline. That test used a source that could
renegotiate its own size, which capture cannot. A forced key frame takes effect
on the next frame and re-sends the parameter sets. The default limit on forcing
is none.

### Constant bitrate spends most of the stream on padding

Synthetic desktop content at 1080p60:

| Content | Total | Filler | Picture |
|---|---|---|---|
| Static page with a moving cursor | 4.75 MB | 87.2% | 0.76 Mbit/s |
| Full-page scrolling | 4.75 MB | 78.2% | 1.45 Mbit/s |
| A busy quarter-screen window | 5.18 MB | 0% | 7.86 Mbit/s |

Constant bitrate pads every frame to the target. A quiet desktop spends 8 Mbit/s
to carry 0.76 Mbit/s of picture. A bandwidth estimator that refreshes only when
the sender is not supply-limited also reads seven-eighths padding as real
demand. Variable bitrate with a target percentage emits no filler: 1.95, 1.98
and 6.20 Mbit/s for the same three cases. In all later runs filler was 0.000% at
every size.

### The bitrate is a target, not a ceiling

At 20 Mbit/s configured, 3440x1440 achieved 20.7 to 35.8 Mbit/s depending on
content, 2580x1080 13.7 to 14.0 Mbit/s, and 1720x720 7.0 to 8.7. Congestion
control must treat the bitrate property as a request, not a bound.

### The frame rate in the caps is the real bitrate control

The encoder budgets bits per frame as the bitrate divided by the frame rate in
the caps, and it ignores timestamps. Caps declaring 60 while capture delivers 20
give a third of the configured rate. Caps declaring 30 while capture delivers
120 give four times it. So the declared rate must track the output refresh.
[`congestion.md`](congestion.md) later corrects the linear reading: with frames
dropped ahead of the encoder at a fixed declared rate, the wire rate follows
roughly the 0.65 power of the delivered rate (half the frames give 62% of the
bytes). Rate control spends the slack on the frames that arrive.

The scaler does not convert frame rate. A different rate downstream of it fails
to link. So the declared rate goes on the source caps, and the filter between
scaler and encoder carries geometry only.

### Coded size, aspect and reordering

The encoder codes 1080 as 1088 and 1800 as 1808 and attaches a crop. 720 and
1440 need none. A client that ignores the crop shows eight rows of encoder
working data and a slight vertical squeeze. The scaler preserves aspect ratio
only when told to, and its correction never reaches the bitstream: the sequence
parameter set of a 16:10 capture scaled to 16:9 has no aspect ratio. So the
coded size, visible size and display aspect travel in the stream configuration
message.

The sequence parameter set declares zero reordering, so a decoder outputs each
picture once it is decoded. It also declares a two-frame decoded picture buffer.
No setting changes that, and it does not delay output.

### The encoder imports one buffer layout

For shared buffers the scaler accepts one memory layout per pixel format, the
compressed one this GPU prefers, and not linear. Capture must choose from the
intersection of the compositor's list and the encoder's list. If the two are
negotiated separately, import fails on any machine where they differ.

### Restating the input size is free; the memory feature is not

This replaces a claim that restating an unchanged size defeats pass-through and
doubles the cost (9.2 to 18.6 ms at full resolution). 3440x1440, 400 access
units per run, three runs per variant, every run at the refresh rate:

| filter caps | median | p90 |
|---|---|---|
| left open, `ANY` | 7.64, 7.64, 7.64 ms | 7.83, 7.80, 7.79 ms |
| `video/x-raw(memory:VAMemory)` with the same size | 7.64, 7.62, 7.57 ms | 7.82, 7.78, 7.78 ms |
| bare `video/x-raw` with the same size | 7.60, 7.64, 7.61 ms | 7.80, 7.81, 7.80 ms |

There is no difference, so the filter is set unconditionally: one branch fewer
and one negotiated memory type. The memory feature decides what is negotiated.
Caps read back off the pads:

- Left open: `video/x-raw(memory:DMABuf), format=DMA_DRM,
  drm-format=NV12:0x0200000010401b04`. A descriptor.
- `(memory:VAMemory)`: `video/x-raw(memory:VAMemory), format=NV12`. A VA
  surface.
- Bare `video/x-raw`: `video/x-raw, format=NV12`. **System memory**, on every
  pad.

Both elements honour "no feature means system memory". That is free here,
because the integrated part shares memory with the CPU, and costly on a discrete
GPU. The filter carries `(memory:VAMemory)`.

### The whole chain, end to end

From `appsrc` to the access unit (scaler plus encoder). Captured at 3440x1440,
four interleaved passes per size, each at about 72 frames per second:

| encode size | median | p90 | p99 | worst |
|---|---|---|---|---|
| 3440x1440, no scaling | 7.61 to 9.15 ms | 7.79 to 9.38 ms | 7.94 to 9.54 ms | 16.75 ms |
| 2580x1080 | 8.99 to 9.82 ms | 9.91 to 9.97 ms | 10.01 to 10.10 ms | 15.37 ms |
| 1720x720 | 5.04 to 5.11 ms | 5.22 to 5.25 ms | 5.36 to 5.43 ms | 11.33 ms |

The three-quarter rung (9.8 ms) costs more than no scaling. The exact half costs
5.1 ms. **Build the ladder from halvings.** `Ladder::new` in
`crates/tachyx-hostd/src/ladder.rs` does.

Latency roughly doubles when frames arrive intermittently: the native size
measures 7.6 ms at 72 frames per second and 15.0 ms over a run averaging 34,
because the part downclocks between frames. Quote a latency figure only with its
frame rate.

### Three shared buffers is the measured minimum

With two, `publish` reaches a state within a second, every time, where no buffer
is free to arm and none can be freed by displacing the pending frame. With three
this never happens. Over about eleven thousand frames the starvation path ran 0
to 56 times per run and always recovered on the same pass. No vertical blank was
missed.

Push then pull makes the chain serial. This was inferred before and is now
measured: pushes and pulls stayed one to one, with no tearing. So the return of
the pull means the scaler is done with the shared buffer.

### Nothing schedules a key frame any more

Both two-second mechanisms are gone: `key-int-max` in the encoder, and a wall
clock in the daemon.

`gst-inspect-1.0` gives `key-int-max` a range of 0 to 1024 on `vah264enc` and
`vaav1enc`. 0 lets the element choose, so 1024 is the nearest to an infinite
group of pictures. DP-2, 2580x1080, 8 Mbit/s, 400 pictures: **0 key frames** at
1024 against 2 at a two-second interval. A test in `encode.rs` reads the value
back off the real element, because GObject clamps an out-of-range value with
only a warning.

Over 1437 pictures, **key frames average 287.2 kB and delta frames 30.2 kB**, a
ratio of 9.5. One key frame per 150 pictures (two seconds at 75 Hz) is 5.4% of
the stream. The 1080p60 sizes in the pacing section (130.4 kB and 16.7 kB) give
5.7%. The latency cost is larger: capture-to-delivery p99 was 18.2 ms with a key
frame every second and **5.5 ms** with one at the start ("Loopback, measured").

The client asks for a key frame when it needs one. Frame identifiers count
encoded pictures and skip none, so a wholly lost frame leaves a hole that the
next picture reveals. The exception is a wholly lost frame followed by **no
picture at all**. A still desktop produces exactly that, because capture blocks.
`repair_if_stale` covers that case only: it fires when the client reported a
datagram missing and nothing has gone out since. A client cannot hold a broken
picture without reporting it, because the receiver report is per datagram and
travels on the reliable stream.

## Input injection

`zwlr_virtual_pointer_manager_v1` version 2 and `zwp_virtual_keyboard_manager_v1`
version 1. An experiment with the idle notifier as the observable proved that
injection reaches the compositor's input path.

- Absolute motion maps onto the bounding box of the whole output layout, which
  need not start at the origin: x from 843 to 5043 and y from 0 to 2940 here.
  Compute it from logical output sizes.
- No high-resolution scroll: no `axis_value120` request at any version.
  Continuous axis values and discrete notches exist.
- The compositor does not derive modifier state from injected keys. The daemon
  tracks modifiers and sends them.
- An uploaded keymap must name its group. It goes over a memory file
  descriptor, ends with a zero byte, and the stated length includes that byte.

### Two virtual keyboards

Physical keys need the host's layout, which is why keys travel as HID usage
codes. On-screen keyboard text must bypass layout. One keymap cannot do both
without live edits to the layout of the focused application. So the seat has
two keyboards. One carries the host's real layout, read from the seat and never
re-uploaded. The other carries a scratch keymap for text. XKB accepts a Unicode
keysym directly (verified for a currency symbol, a CJK ideograph and an emoji
outside the basic planes). An upload pushes an event to the focused
application, and some applications reset input method state on it. So the
scratch keymap accumulates codepoints and is re-uploaded only for a new one.

## Transport

QUIC datagrams for video, one reliable stream for control and input.

quinn's congestion controller covers datagrams. **Read** in three places in its
source, then confirmed by offering 5000 datagrams at once: datagram packets are
acknowledgement-eliciting, gated by the window, counted in flight, tracked for
loss and never retransmitted. The earlier conclusion that the bandwidth estimate
therefore comes free is wrong (see "quinn's bandwidth estimate is not a
bandwidth estimate").

A successful send call does not mean the datagram was sent. In the burst test
2166 of 5000 vanished with no error. So the send buffer holds a whole key frame
and the space is checked before a frame is queued. A frame is dropped whole, by
us.

### The send buffer must hold a key frame, not two frames

- quinn discards the **oldest first**, not "arbitrary ones" as first written.
  The oldest datagrams are the head of the frame the client is about to decode.
- "About two frames", as first written, is ambiguous in a way that decides
  whether the stream starts. A 1080p key frame is 130,427 bytes in 92 datagrams.
  A P frame at 8 Mbit/s and 60 Hz is 16,673 bytes in 12. Two P frames is 33 KiB,
  so every key frame is refused, the client never synchronises, and no picture
  appears. The buffer is one key frame scaled by pixel count, plus the
  per-datagram accounting, plus half a key frame: 203,785 bytes at 1920x1088.
  `video_send_buffer` (`crates/tachyx-transport/src/video.rs`) now also counts
  the key frame's parity at the 50% ceiling.

### Reaching quinn's eviction path is fatal

Both profiles, quinn-proto 0.11.17. The eviction loop in `Datagrams::send`
subtracts a dropped datagram's length from `payload_bytes` twice: in
`DatagramBuffer::pop_front` and again in the loop body.

- Debug, or any build with overflow checks: the next drain panics with
  `attempt to subtract with overflow` at `connection/datagrams.rs:203`. The
  panic poisons the connection's mutex. Every later use panics, and the drop
  panics during cleanup, which aborts the process.
- Release: `payload_bytes` wraps. The connection reports zero free space for
  ever, the gate refuses every frame, and video stops with no error. The next
  datagram hits `expect("datagrams.outgoing.payload_bytes desynchronized")`.

Found at saturation, where it killed the connection. To reproduce, fill the
queue past the eviction threshold and let it drain. The gate is what keeps the
connection alive.

### The free-space report understates

`Connection::datagram_send_buffer_space()` subtracts one entry of accounting.
Eviction charges one **per queued datagram**, so the two disagree by 32 bytes
per queued datagram. With a 1080p key frame queued (92 datagrams), 71,854 bytes
are reported free: the report implies 49 more datagrams, and the queue takes 47.
Trusting the report evicts.

The gate computes the gap exactly. This transport is the only producer and quinn
takes from the front, so the queue holds the tail of what was pushed. The tail
whose sizes sum to the reported payload bytes gives the entry count. If no tail
sums, something removed a datagram from the middle, and the gate refuses until
the queue drains.

### The receive buffer evicts the same way

`DatagramState::received` silently drops the oldest queued datagram to make
room. Its accounting is correct, so nothing panics. A client with a 29,991-byte
receive buffer receives 20 of a key frame's 92 datagrams, with no error.
quinn's default of 1,250,000 bytes is fine. Anything below a key frame plus the
application's drain lag is not. Send and receive use the same figure. At
203,785 bytes, a key frame left unread for 100 ms arrives 92 of 92.

### Loopback, measured

180 frames at 60 Hz, a key frame every 60, real sockets and handshake,
1418-byte payloads. Three runs. The third ran with the machine compiling.

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

Re-run on the newer shell to price the scheduled key frame. Three runs each on
an idle machine:

| key frames | median | 99th percentile | worst |
|---|---|---|---|
| one every 60 frames | 1.49, 1.30, 1.46 ms | 18.36, 17.96, 18.38 ms | 26.33, 26.36, 27.33 ms |
| one at the start | 1.36, 1.42, 1.33 ms | **5.50, 5.09, 5.86 ms** | 26.49, 26.43, 27.19 ms |

Only the p99 moves, by 12.5 ms. The worst case is the opening key frame, which
both shapes pay. The cause is not the key frame's 26 ms span. `Pacer::release`
charges bytes whether the caller waits or not, so the two delta frames behind a
key frame pay back its debt: three frames in sixty at one key frame a second.
Bytes carried: 26.74 Mbit against 24.92. The test now sends the second shape, as
the host does.

Capture to delivery starts when the frame was due to be sent, so it holds sender
scheduling and no encoder time. Idle, the transport costs about 1.5 ms of the 10
to 20 ms budget. Under load it costs up to 10 ms, all of it scheduling, because
the round trip moves with it.

Driven as hard as the gate allows for 1.5 seconds: 1.1 to 3.3 Gbit/s, round trip
29 to 209 µs, BBR pacing rate 2.9 to 3.4 Gbit/s. About 1% of arriving datagrams
are lost to kernel UDP drops. Eviction would have ended the connection instead.
Of 15,968 frames offered, the gate refused 4616 whole. The client counted 4704
missing (the refusals plus kernel drops) and discarded 4679 complete delta
frames that referred to them. That is correct, because a refused frame is one
the reference chain has already passed. Without deliberate overrun the gate
refuses nothing.

Other facts:

- `pacing_rate` is `Some` only under BBR. Cubic and NewReno report `None`.
- Datagram frames go into a packet before stream frames, so control traffic
  cannot starve video within a packet. Both share the window.
- **Read** from quinn 0.11.11 and quinn-proto 0.11.17: `max_idle_timeout` 30 s;
  `initial_rtt` 333 ms, which seeds loss detection, so a loss goes unsuspected
  for a third of a second on a sub-millisecond link; `initial_mtu` and
  `min_mtu` 1200; no keep-alive; no acknowledgement frequency configuration;
  `datagram_send_buffer_size` 1 MiB (about eight 1080p key frames of queue);
  Cubic. `AckFrequencyConfig::max_ack_delay` is clamped to at least the peer's
  minimum and at most the greater of the path round trip and 25 ms, so the 2 ms
  asked for holds only on a local link.
- No public API fails fast. `Connection::send_datagram` always passes
  `drop = true`, so its `Blocked` result is unreachable. `send_datagram_wait`
  waits instead and favours old datagrams over new ones, which is backwards for
  live video. The gate is the only defence.
- quinn and rustls default to different cryptographic providers. With quinn on
  `rustls-ring` and rustls on `ring`, the lockfile has no `aws-lc-rs`.

### Recovery on demand (superseded)

On loopback, recovery was a key frame request and an IDR, with no parity:
video is never retransmitted, neither intra-refresh nor LTR is reachable, and a
round trip under a millisecond made parity a continuous cost. Sunshine and
Moonlight spend a fifth of their bandwidth on parity because a wide-area round
trip exceeds a frame period. **Superseded** by "Loss resilience, re-derived for
this link": every frame now carries parity, and recovery on demand is the
backstop. A frame that loses every packet leaves no trace in the sequence
numbers, so the host numbers every encoded picture and skips none
([`protocol.md`](protocol.md)).

### Segmentation was off for every session, including direct ones

`RoutingSocket::max_transmit_segments` answered 1. A relay reads a nine-byte
header and forwards the rest, so a segmented transmit to a relay arrives with a
header on the first piece only. Both production endpoints use this socket, so
the five sessions in six that never touch a relay lost segmentation too. The
socket underneath reports **64**.

`try_send` now splits relayed transmits only. The cost falls on the pacer's
25,000-byte burst allowance, about seventeen datagrams: seventeen system calls
instead of one, and four times that at a raised ceiling. That is CPU time on the
capturing machine, not latency. Not measured end to end, because every harness
binds a plain host socket. Only the first datagram of a relayed transmit may
report a failure: quinn answers `WouldBlock` by re-offering the whole transmit,
which would send the earlier datagrams twice.

## The picture threads need the realtime class

The loopback p99 moves from 2.5 ms to 7.9 ms on a busy machine, and the worst
case from 2.8 ms to 10.5 ms. The round trip moves with it, so the cause is the
scheduler, not the network. Nothing set a thread priority before `tachyx-sched`.

### A nice level does not fix it; the realtime class does

`crates/tachyx-sched/examples/wake.rs` sleeps 1 ms and then runs, as a thread on
this path does. 20,000 wake-ups against 64 spinning processes on 16 hardware
threads, each process in its own session:

| | p50 | p90 | p99 | worst |
|---|---|---|---|---|
| ordinary | 0.006 ms | 0.016 ms | 2.977 ms | 46.409 ms |
| nice -11 through RealtimeKit | 0.007 ms | 0.017 ms | 2.987 ms | 30.115 ms |
| `SCHED_RR` 5 through RealtimeKit | 0.006 ms | 0.015 ms | **0.018 ms** | **0.047 ms** |

Two repeats of the shipped path: p99 0.018 ms both times (3.000 and 2.990 ms
unraised), worst 0.150 and 0.034 ms (99.724 and 53.169 ms unraised).

**The nice level buys nothing above p90.** `kernel.sched_autogroup_enabled` is
1 here and on every desktop distribution. The fair scheduler shares the CPU
between session groups first, so a nice level orders a thread only against its
siblings, not against a compile in another terminal. An earlier run spun the
load inside the same process, in one autogroup, and made nice look effective.
The realtime class sits above the fair scheduler and reaches the tail. PipeWire
asks the same broker for it.

### RealtimeKit is the only unprivileged way in

`ulimit -e` and `ulimit -r` are 0 soft and 0 hard. So `setpriority` below 0 and
`sched_setscheduler` fail for an ordinary login, and a `setpriority` fallback
would always fail. `rtkit-daemon` runs on the system bus wherever PipeWire runs,
and the host needs PipeWire for sound.

Three bounds, none of them ours, keep a realtime thread from taking the machine
down:

- `kernel.sched_rt_runtime_us` is 950,000 of 1,000,000: 5% of every CPU stays
  for ordinary threads.
- `RLIMIT_RTTIME` is 200 ms, and the broker requires it. It is the kernel's
  dead-man switch for a thread that holds a CPU without blocking.
- The priority is 5 of the broker's 20, so the sound server and the kernel's
  interrupt threads still outrank us.

Raised: capture, encode, the sound pump, the host's release loop, and both
client runtime workers. Each blocks within a frame.

### The client sees the host's half

`Heartbeat.pipeline_micros` was zero. It now carries the median capture-to-wire
time over the heartbeat interval, measured in the release loop once the pacer
releases the picture's last datagram. It is an elapsed time, so it needs no
clock agreement, which is why Sunshine reports it the same way.
`tachyx-hostd status` prints it.

## Discovery

Not built. The tree has no mDNS code, so nothing here was run.

The design: the host advertises over mDNS. On Android the platform's service
discovery takes the multicast lock itself for foreground applications, so no
permission is needed. The multicast permission would add an implied hardware
requirement that hides the application from devices without Wi-Fi in the store,
and a held lock wakes the radio on every beacon interval.

## Client

No prior art exists. There is no Flutter Moonlight or Sunshine client, and every
Flutter video plugin surveyed expects a URL or a file. RustDesk on Android
decodes into a byte buffer, converts on the CPU and rebuilds a Dart image every
frame. Measured costs force a different path per platform.

### Android

Decode with the platform decoder through the `ndk` crate. Its media API is safe
and compiles for `aarch64-linux-android` under `forbid(unsafe_code)`. It decodes
straight into a surface, so no frame passes through our code.

Present through Flutter's surface producer, via a short Kotlin class
(`VideoSurface.kt`). On current Android it hands decoder buffers directly to the
compositor's Vulkan backend. The alternative, `irondash_texture`, uses the older
surface texture API. Flutter's own source describes that OpenGL-to-Vulkan
interop as one device-to-device copy per frame, kept as a migration fallback.

One unsafe call cannot be avoided: turning the Java surface into a decoder
surface, because no type proves that a Java reference is a live surface. It
lives in a leaf crate, like the host's descriptor import.

### Linux

Decode with GStreamer, which the host already depends on. GStreamer elements
upload and convert on the GPU and hand Flutter a texture, so we write no
graphics code.

Flutter's Linux embedder has no interface for shared buffers and no proposal for
one. It works anyway, because the texture callback runs on the render thread
with Flutter's GL context current. Two calls pass the raw display and context
handles. They are the only unsafe code in the Linux client.

The client must run on Wayland. Under X11 the embedder reads the whole window
back from the GPU every frame: several megabytes and a pipeline stall per frame.

This is the highest risk in the project. The texture path depends on a crate
whose graphics payload has roughly eleven thousand lifetime downloads, one
maintainer, a stable release from 2023, and no issue ever filed about its OpenGL
support. It is about a hundred lines. Be ready to take ownership of it.

### The Linux path, measured

Release build. A `vah264enc` test pattern at 1920x1088, decoded and drawn in a
Flutter window on Wayland, on both renderers.

The pipeline is
`appsrc(alignment=au) ! h264parse ! vah264dec ! video/x-raw(memory:DMABuf) !
glupload ! glcolorconvert ! appsink(memory:GLMemory, RGBA, texture-target=2D)`.
Three caps are load-bearing, and each turns a silent regression into a
negotiation failure: `memory:DMABuf` after the decoder, `texture-target=2D` at
the sink, and `format=RGBA`. `glupload` picks `DirectDmabufExternal`: the
decoder's buffer becomes an `EGLImage` and comes out as a
`GL_TEXTURE_EXTERNAL_OES` texture. `glcolorconvert` makes `GL_TEXTURE_2D` RGBA in
one shader pass. That pass stays, because Flutter needs `GL_TEXTURE_2D` and
`GL_RGBA8` and the decoder emits tiled NV12. It costs GPU time, not a copy.

| | |
|---|---|
| Decode to texture ready, 60 Hz feed | median **0.63 ms**, p95 **0.85 ms**, worst **1.34 ms** |
| Texture ready to Flutter's rasteriser | nothing measurable |
| End to end | decode plus the wait for the compositor's next frame, which every client pays. Lowest seen **1.87 ms**, highest one frame interval more |
| Sustained throughput, fed as fast as accepted | **430 frames a second** at 1920x1088, seven times what 60 Hz needs |
| Steady-state drops | none. Frames replaced before Flutter asks for them fall to zero after the first second |

CPU per frame at 1920x1088, all threads counted. This proves that nothing is
copied:

| path | CPU per frame |
|---|---|
| decoder to shared buffer to texture, as above | **1.0 ms** |
| decoder to system memory, `videoconvert` on four threads to RGBA | 5.0 ms |
| the same, then uploaded to a texture, which is what a pixel buffer fallback costs | 5.6 to 11.7 ms |

A CPU fallback costs five to twelve times the CPU per frame and three to eleven
milliseconds of latency. It is not an option at 60 Hz.

Impeller is the default. The embedder sets `enable_impeller = TRUE` and logs
`Using the Impeller rendering backend (OpenGLESSDF)` with no flag. Both
renderers draw correct colour. Skia needs `FLUTTER_ENGINE_SWITCHES=1` and
`FLUTTER_ENGINE_SWITCH_1=enable-impeller=false`, because the runner's arguments
go to the Dart entry point, not to the engine.

`irondash_texture` 0.5.0, the stable release, carries the Linux OpenGL payload,
and its vtable still matches. `FlTexture` became a GObject interface and
`FlTextureGL` derives from `GObject` directly, but `FlTextureGLClass` is still
`GObjectClass` plus one `populate` pointer, byte for byte. The embedder uses the
texture's address as its identifier, which is what the crate returns. No fork is
needed.

Two embedder facts not written down anywhere else:

- Without `alignment=au` on the `appsrc` caps, the parser holds every frame for
  one frame period, until the next one arrives. That was 16 ms of the 17 ms
  first measured.
- A pipeline that cannot negotiate says so only on the bus. The present path
  records the first such message and exposes it.

### Consequences for the host

1. Encoded dimensions had to be multiples of sixteen, because Flutter's Android
   present path ignores the H.264 crop (1080 is coded as 1088). Replaced: the
   stream configuration carries the coded and visible sizes, and the Flutter
   view applies the crop (`app/lib/src/ui/video_texture_view.dart`). Ladder
   rungs are halvings rounded to even (`crates/tachyx-hostd/src/ladder.rs`).
2. The parameter sets repeat before every key frame, not once out of band. The
   client needs no configuration step, and it recovers alone from a reconnect, a
   resolution change, or a parameter set lost on a path that does not
   retransmit.
3. The sequence parameter set declares zero reordering. Without it a decoder
   buffers frames, and no client setting removes that latency. We control the
   encoder, so we state it instead of rewriting the bitstream on the client as
   Moonlight must.

### Input

Asked for single and double taps, Flutter's gesture recogniser holds the
gesture arena until the double-tap timer expires: a measured 310 ms before a
single tap, and no single-tap callback at all when a double tap happens. The
client uses one raw pointer listener and its own state machine, which reports a
tap the instant the finger lifts.

Keys are read before any text field can take them. That also keeps the back
gesture inside the session and stops tab and the arrows from moving focus.
On-screen keyboard text arrives as edit deltas, so composed text arrives as one
committed string.

## Pacing and loss on a real wireless link

Loopback has a sub-millisecond round trip and no queue we do not own. A Galaxy
S25 over Wi-Fi 6, sixteen seconds of streaming: 859 packets in, 24 lost, 40
late; 63 frames in, 8 missing, 12 undecodable; round trip 13.91 ms; the host
gated 179 frames and sent 64.

The probe is a real quinn host and a real quinn client with the shipped
transport configuration. A userspace relay between them sets the one-way delay,
a bottleneck rate with a finite drop-tail queue, a loss rate and a per-packet
jitter. quinn's congestion controller, pacer and datagram queue and the shipped
gate are all real. Frame sizes are the measured ones: 130,427 bytes for a 1080p
key frame and 16,673 for a P frame. The client requests a key frame over the real
control stream when reassembly cannot continue, and the host answers under the
shipped 250 ms budget. Without that, a run measures only the first break.

### The air hop, measured

1200 pings of 1400 bytes at 60 Hz from the laptop to its access point, one of
the two air hops the phone is behind:

| | |
|---|---|
| round trip | min 2.12, median 3.26, p90 6.48, p99 141, worst 243 ms |
| delay above the fastest probe | median 1.14, p90 4.36, p95 7.00 ms |
| probes more than 2 ms above the floor | **22.92%** |
| probes more than 5 ms above the floor | 8.33% |
| probes more than 10 ms above the floor | 3.42% |
| probes more than 50 ms above the floor | 2.25% |

Nothing was lost. The medium delays packets instead of dropping them, about a
quarter by more than 2 ms. Compare the original 2 ms reorder window, below.

This hop is 3.26 ms at the median, so about 10 ms of the phone's 13.91 ms is the
second air hop: the phone's radio waking. **Inferred:** that is Wi-Fi power
save, not a queue that sending less would shorten.

### quinn paces, and its pacer does nothing for us

**Read** in `quinn-proto` 0.11.17, then measured. `Connection::poll_transmit`
checks the pacer before every UDP datagram, so it covers application datagrams.
`Pacer` is a token bucket with capacity `window * 2 ms / smoothed_rtt`, clamped
to between 10 and 256 MTUs, refilling at `1.25 * window / smoothed_rtt`. It
reads the congestion window, not BBR's pacing rate: `calculate_pacing_rate`
computes one and `Pacer` never reads it. With BBR's steady-state window gain of
2.0, it paces at about **2.5 times the bandwidth estimate**, plus a 2 ms burst.

With no bottleneck and a 13.91 ms round trip, a 1080p key frame's 92 datagrams
arrive spread over **0.08 ms** at the median, and a 12-datagram P frame over
0.02 ms. That is 130 kB in 80 µs, about 13 Gbit/s. The pacer never engages.
**We need our own pacer.** It is built: `crates/tachyx-transport/src/pace.rs`.

### quinn's bandwidth estimate is not a bandwidth estimate

This overturns the claim in "Transport" that "the bandwidth estimate comes free
from the transport, and a delay-based estimator of our own is not needed on a
local network". On a link with headroom the estimate does not exist.

`BandwidthEstimation::on_ack` updates only when
`!app_limited && self.max_filter.get() < bandwidth`. `Connection::app_limited`
is set after every `poll_transmit` to `buf.is_empty() && !congestion_blocked`.
That tells whether the *last* poll had data, not whether the acknowledged packet
left while application limited. A 60 Hz sender fills the queue in about 100 µs
and then idles for 16 ms, so every acknowledgement finds `app_limited` true and
no sample is taken. The guard `max_filter.get() < bandwidth` also means the
filter only rises and never decays.

Sampling `Connection::congestion_state().metrics().pacing_rate` every 50 ms. A
9 Mbit/s stream on a 100 Mbit/s path that drops to 6 Mbit/s at ten seconds:

| t | reported pacing rate |
|---|---|
| 0 to 4 s, path 100 Mbit/s | **0.00 Mbit/s** |
| 5 to 10 s, path 100 Mbit/s | 0.76 to 0.95 Mbit/s |
| 11 to 30 s, path 6 Mbit/s | 7.31 Mbit/s, with excursions to 12.19 |

It reads too low with headroom and too high without. A separate 20-second run at
9.2 Mbit/s on a 100 Mbit/s path with a deep queue: all 400 samples were
**0.0 Mbit/s**.

`Ladder::observe` was fed `pacing_rate * 85 / 100` through `connection_target`.
A zero target drove bits per pixel below `STARVED` and stepped the ladder down
every two seconds to its bottom rung. Only real congestion made an estimate
exist. **We need our own estimator, because quinn's is absent whenever the link
is fine.** The ladder now takes its target from the delay-based controller in
`tachyx-congestion`, driven by the client's receiver report
(`set_target(self.controller.video_target())` in
`crates/tachyx-hostd/src/session.rs`).

### Why a burst fails: the buffer, not the rate

100 Mbit/s path, 13.91 ms round trip, no loss or jitter, 20 seconds at 60 Hz, a
key frame every two seconds, frames sent as bursts. Only the bottleneck's
drop-tail depth varies:

| bottleneck buffer | frames delivered | packets dropped by the queue |
|---|---|---|
| 16 kB | 0.1% | 8067 |
| 32 kB | 12.0% | 5549 |
| 64 kB | 12.1% | 3628 |
| 96 kB | 18.8% | 1458 |
| **128 kB** | **97.4%** | 1 |
| 192 kB | 97.9% | 0 |

The step is at one key frame. 130,427 bytes offered in 80 µs either fit or do
not. At 100 Mbit/s the buffer drains 32 kB in 2.6 ms, and we fill it in 0.1 ms.
The link rate does not matter. The buffer is a per-station queue inside an
access point, and we cannot see it. **So the transport must not depend on its
depth.**

A burst into a 32 kB buffer, with the key frame capped:

| key frame | frames delivered | peak backlog |
|---|---|---|
| 130,427 B, as measured | 0% to 68% depending on the seed | 32,739 B, full |
| 65,000 B | 0% | 32,752 B, full |
| 40,000 B | 6.2% | 32,734 B, full |
| **25,000 B** | **100%**, worst latency 13.47 ms | 24,637 B |

The spread on the first row is bistability. One destroyed key frame causes a
request for another, which bursts into the same full buffer, and the session
never recovers. Three seeds of one configuration delivered 51.7%, 7.3% and 0.0%.

### Pacing works, at the path's rate

Pacing over a fixed slice of the frame interval is wrong. On an unbottlenecked
path, spreading each frame over 0.9 of a frame interval delivers 100% and moves
the median latency from **9.21 ms to 23.64 ms**. A frame is useless until its
last datagram, so 14 ms of spread costs 14 ms, of a 10 to 20 ms budget.

Pace at a byte rate near the path rate. 50 Mbit/s path, 32 kB buffer, 20
seconds:

| how the frame is offered | delivered | packets lost | median latency | worst |
|---|---|---|---|---|
| burst | 1.3% | 5446 | 12.20 ms | 33.77 ms |
| paced at 10 Mbit/s | 0% | 0 | key frames took 104 ms and timed out | |
| paced at 20 Mbit/s | 0% | 0 | key frames took 52 ms and timed out | |
| **paced at 40 Mbit/s** | **100%** | **0** | **13.44 ms** | 37.92 ms |
| paced at 80 Mbit/s | 0% | 1713 | overran the buffer again | |

100 Mbit/s path, same 32 kB buffer, three seeds each:

| | delivered | packets lost | median | p90 | p99 | worst |
|---|---|---|---|---|---|---|
| burst | 51.7%, 7.3%, 0.0% | 2897, 5887, 6328 | 10.6 to 11.3 ms | 11.1 to 14.0 | 15.3 to 42.6 | 46 to 70 ms |
| **paced at 80 Mbit/s** | **100%, 100%, 100%** | **0, 0, 0** | 11.6 to 11.8 ms | 12.4 to 12.7 | 13.4 to 14.3 | 24.6 to 25.0 ms |

Pacing at 80% of the path rate costs about **1 ms of median latency** and lifts
delivery from 0 to 52% of frames to all of them. Every higher percentile
improves, because an overfilled queue delays everything behind it: p99 from up
to 42.6 ms to 14.3 ms, worst from 70 ms to 25 ms.

The window is narrow. Too fast overruns the buffer. Too slow makes a key frame
outlast the receiver's wait. On a 25 Mbit/s path the best rate was 20 to
25 Mbit/s (98.2% and 99.1%), so "pace at about the path rate" holds across a
factor of four. Pacing at quinn's rate collapsed an unbottlenecked stream to
45.8%: the estimate falls, the pacer slows, the sender becomes more application
limited, and the estimate falls again.

### The key frame is the whole problem

At 8 Mbit/s a 130 kB key frame is 130 ms of the average budget in one 16 ms
slot. Pacing moves the queue to us but does not reduce the bytes. Paced at
40 Mbit/s a key frame still spans 26.6 ms at the client, and at 25 Mbit/s
43.8 ms. Ways out, in order:

1. **Cap the key frame.** At 25,000 bytes it survives a burst into 32 kB. The
   encoder has no direct control for this. The coded picture buffer floor is
   half the bitrate, and the bitrate is a per-frame budget divided by the
   declared rate. This is the cheapest lever. No direct cap exists in the tree.
2. **Spread it over more than one frame interval**, with the receiver's deadline
   raised to match. On a 25 Mbit/s path a key frame paced over 70 ms delivered
   97.2% against a 120 ms deadline. Against the shipped 50 ms deadline, on a
   50 Mbit/s path, a key frame paced over 52 ms delivered **0%**: each timed out
   1 or 2 ms before its last datagram. Frames inside the span are never encoded,
   so they cost no frame identifier and no reference chain.
3. **Intra-refresh**, which removes key frames. GStreamer does not expose it on
   either GPU. `VAEncMiscParameterTypeRIR` through `cros-libva` is the route.
   **Not measured.** It is still a large undertaking, but every measurement
   here raises its value.

**Inferred:** the order follows from 1 and 2 being settings and 3 being a new
encoder backend.

### Loss resilience, re-derived for this link

Recovery on demand rested on this: "On a local network the round trip is under a
millisecond, so parity on every frame would be paying continuously for
something that costs one round trip and one IDR when it happens." The reasoning
holds and the premise does not. Here the round trip is 13.91 ms and a 60 Hz
frame period is 16.67 ms.

Frame survival, closed form, independent loss at the measured 2.79%:

| | delta, 12 datagrams | key, 92 datagrams |
|---|---|---|
| no parity | 0.712 | **0.074** |
| 20% parity | 0.9994 | 1.0000 |

An unprotected key frame survives **seven times in a hundred**, and so does the
key frame requested to replace it.

Monte Carlo over a 60 fps stream: twelve runs of 60 seconds, the chain rule (a
lost frame makes every later frame undecodable until a key frame lands), and the
shipped 250 ms key frame budget. `hold` is how long the client keeps an
incomplete frame open. At 2.79% independent loss, hold of one frame period:

| scheme | frames shown | freezes | median freeze | bandwidth |
|---|---|---|---|---|
| none (recovery on demand only) | 1.5% | 0.27/s | 2533 ms | 11.89 Mbit/s |
| retransmit only | 5.2% | 0.37/s | 2017 ms | 12.00 Mbit/s |
| 10% parity everywhere | 98.3% | 0.38/s | 33 ms | 10.37 Mbit/s |
| **20% parity everywhere** | **99.9%** | 0.03/s | 33 ms | 10.73 Mbit/s |
| 20% on delta, none on key | 88.4% | 0.04/s | 2300 ms | 11.07 Mbit/s |
| 20% on delta, 5% on key | 99.8% | 0.03/s | 33 ms | 10.72 Mbit/s |

Hold of two frame periods:

| scheme | frames shown | median freeze | bandwidth |
|---|---|---|---|
| none (recovery on demand only) | 1.5% | 2533 ms | 11.89 Mbit/s |
| **retransmit only** | **99.3%** | 850 ms | **8.81 Mbit/s** |
| 20% parity everywhere | 99.9% | 33 ms | 10.73 Mbit/s |
| 20% parity and retransmit | 100.0% | 0 ms | 10.69 Mbit/s |

- **Retransmission fits only with a hold of two frame periods.** A gap inside a
  paced frame shows within a fraction of a millisecond. A hole at the tail shows
  only with the next frame's first datagram, a frame period later. So the repair
  lands 14 to 31 ms after the loss: 5.2% shown at one period, 99.3% at two.
  Retransmission is cheap but adds latency.
- **Parity alone needs no extra hold**, because the repair travels with the
  frame. Its freezes last one frame, not a second.
- **Key frames need parity, because of our frame sizes.** Sunshine can skip it.
  Our key frame is 92 datagrams against a delta frame's 12, so it is the most
  exposed frame. With no key frame parity: 88.4% shown and a 2.3 second median
  freeze. 5% parity (five datagrams) gives 99.8%. Size parity by datagram count,
  which a percentage does.
- **Bursty loss defeats each scheme alone.** The same 2.79% in runs of three: no
  protection 21.2%, retransmission 75.2%, 20% parity 81.9%, both 97.2% at
  11.38 Mbit/s.

Bandwidth: at 8.5 Mbit/s of picture, 20% parity is 1.7 Mbit/s. A recovery key
frame is 130 kB (1.04 Mbit), and recovery on demand spends 3.4 of them a second
at this loss, which is 3.7 Mbit/s. **No parity costs more**: 11.89 Mbit/s for
1.5% of frames shown, against 10.73 Mbit/s for 99.9%.

Built: `Parity` (`crates/tachyx-proto/src/packet.rs`) opens at 20%
(`MEASURED`), follows the reported loss between 5% and 50%, adds half again for
key frames, and drops to none only after a run of clean reports (`QUIET`).

### Slices buy nothing until the receiver changes

`num-slices = 1`. The latency measurement (7.30 against 8.81 ms mean, 23.74 ms
worst) stands. Also, **slices buy nothing with this receiver.** **Read** from
the source: `Reassembler` delivers complete frames only, and the first
condition in `Partial::complete` requires `slices.len() == last + 1` and every
slice full. Slicing would need three changes: `num-slices`, a receiver that
hands on partial access units, and decoder concealment. Only the first is a
setting.

What slices would save if the rest were done, from each damaged frame's arrival
record in the runs above:

| the damaged frames of | intact at 2 slices | at 4 | at 8 | at 16 |
|---|---|---|---|---|
| a 100 Mbit/s path with a 128 kB buffer, 13 frames | 50% | 75% | 88% | 94% |
| a 50 Mbit/s path with a 32 kB buffer, 24 frames | 15% | 44% | 51% | 63% |
| a 25 Mbit/s path with a 32 kB buffer, 50 frames | 2% | 4% | 16% | 23% |

The rows are ordered by bottleneck pressure. With scattered loss, four slices
keep three quarters of the picture. A queue overrun takes the tail of the frame
contiguously, and two slices then save 2%. Slices help against **independent**
loss and barely against **queue** loss, which is what bursts produce.

**Recommendation, firm: do not add slices.** They cost 1.5 ms of encoder
latency, need partial-frame handling in the receiver and the decoder, and
recover at best a fifth of a picture against this link's loss. Whole-frame
parity recovers all of it for less. `slice_id` 255 and the spare flag bits stay
reserved for parity.

### The two millisecond reorder window was the worst constant

`REORDER_WINDOW` was 2 ms, documented as "a reordered packet on a link whose
round trip is under a millisecond arrives immediately". On this link **22.92% of
packets on one air hop arrive more than 2 ms behind the fastest.**

Probe with no loss, paced so that no queue overruns, and 3% of packets held up
to 10 ms as a link-layer retry does. 20 seconds, 1200 slots:

| reorder window | frames delivered | frames dropped | late packets | of those, ones that would have completed the frame |
|---|---|---|---|---|
| **2 ms, the old value** | **1.8%** | 384 | 5177 | 384 |
| 5 ms | 8.3% | 241 | 2906 | 241 |
| 10 ms | 46.8% | 50 | 350 | 50 |
| **15 ms** | **94.6%** | 5 | 4 | 4 |
| 25 ms | 94.9% | 4 | 4 | 4 |

The sweep receiver is the shipped `Reassembler` with two constants lifted into
parameters. It was checked against the shipped one over 3913 datagrams and 300
frames, with 2% dropped and 5% held up to 10 ms: the two agreed on every frame
completion and on all seven counters. On that gentle link the shipped receiver
delivered **0 frames of 300** (164 dropped, 136 undecodable).

The jitter-only link lost nothing. All 384 dropped frames were discarded while
their completing datagram was in flight. That explains the 40 late against 24
lost in the phone measurement.

Widening is free. The median is flat at 11.85 to 12.13 ms, and the p90
**improves** from 24.25 ms at 2 ms to 18.80 ms at 15 ms, because a dropped frame
becomes a key frame request and a chain break. The window applies only to a
frame already known to be damaged. An undamaged frame is delivered on its last
datagram. The ceiling is one frame period, after which the receiver's floor has
moved past.

**Inferred:** derive the window. 15 ms suits 60 Hz and not 120. One frame period
is the principled figure, and the client knows it from `StreamConfig`. Built:
`REORDER_WINDOW` is now a 15 ms floor, and the client sets one frame period
(`Reassembler::set_reorder_window`, called from
`crates/tachyx-client/src/session.rs`).

`abandon_below` decided every frame under a completed one at once, even one
datagram short inside its window. Letting those frames keep their window lifts
delivery from 1.8% to 4.2% at 2 ms, far below widening (94.6%). **Not worth the
complexity alone.**

### The gate closed on the wrong question

This explains "gated 179 frames and sent 64". The gate asked
`can_send_bytes(key_frame_estimate())` before every frame: room for 130 kB in a
204 kB buffer. So it shut at 74 kB queued, about five P frames. **Inferred:**
ask for the next frame's size, and ask for a key frame's room only before a key
frame. Built: the host gates on `delta_estimate()`
(`crates/tachyx-hostd/src/session.rs`). A key frame that does not fit is refused
when it arrives.

### What WebRTC does

**Read**, not run: libwebrtc at tip, cross-checked against seven release
branches. Two of the recommendations below match it, and one deliberately does
not.

- **It paces at a byte rate with a burst allowance.** `PacingController` grants
  `min(kDefaultBurstInterval, kMaxBurstSize / rate)`: 40 ms
  (`modules/pacing/pacing_controller.h`, `PacerConfig::kDefaultTimeInterval` at
  tip) or 63,000 bytes, crossing over at a pacing rate of 12.6 Mbit/s. At a
  10 Mbit/s target and the usual pacing factor of 2.5, a 130 kB key frame takes
  **41.6 ms**, about two and a half frame intervals. That is neither one
  interval nor a burst, and it is the shape of recommendation 1.
- `queue_time_limit` (2 s) is inert here. At 25 Mbit/s it needs 6.25 MB queued,
  and our key frame is 130 kB.
- **It caps key frames for VP8, not for H.264.**
  `LibvpxVp8Encoder::MaxIntraTarget` feeds `VP8E_SET_MAX_INTRA_BITRATE_PCT`,
  1800% of the per-frame budget at 60 fps. `H264EncoderImpl` sets
  `iMaxBitrate = UNSPECIFIED_BIT_RATE` and `uiMaxNalSize = 0`, so H.264 key
  frames are uncapped, with only the frame dropper and the pacer behind them. We
  are on the uncapped side (recommendation 3).
- **It protects key frames more.** The parity count is
  `(num_media_packets * protection_factor + 128) >> 8`
  (`modules/video_coding/utility/...`, `forward_error_correction.cc`). The key
  frame factor is boosted by
  `BoostCodeRateKey = max(2, packetFrameKey / packetFrameDelta)` with
  `_scaleProtKey = 2.0` (`media_opt_util.cc`). At our 92-to-12 ratio the boost
  is 7, which hits the clamp: 40 to 50% on key frames, 20 to 32% on delta
  frames. Our Monte Carlo agrees.
- It turns FEC off above a 200 ms round trip (`kMaxRttTurnOffFec`, local to
  `BitRateTooLowForFec`), so on this link WebRTC carries parity and
  retransmission.
- Its NACK list tolerates `kMaxReorderedPackets = 128` on a 20 ms
  `kUpdateInterval` (`modules/video_coding/nack_requester.{h,cc}`), and its
  retransmission window is `kRetransmitWindowSizeMs = 500`. Our receiver then
  gave up 2 ms after it suspected a gap.

### Recommendations

| # | Recommendation | Status |
|---|---|---|
| 1 | **Pace in the transport at a byte rate.** Spread a frame's datagrams over `bytes * 8 / rate`, where `rate` is a target the session owns. Pacing over a fixed slice of the frame interval costs the whole slice. Measured cost at the right rate: about 1 ms of median latency. Benefit on a 32 kB bottleneck: from 0 to 52% of frames to 100%. | Built (`pace.rs`: burst allowance `MAX_BURST` 25,000 bytes or `BURST_SPAN` 15 ms) |
| 2 | **Do not take that rate from quinn.** `pacing_rate` is zero on a healthy link and never falls on a bad one. Pace at a multiple of the encoder's configured bitrate, with a floor that keeps a key frame inside the receiver's deadline. | Built: `rate_for` paces at `PACE_PERCENT` (250%) of the bitrate, floored so a picture leaves within `FRAME_SPAN` (35 ms). The delay-based controller sets the bitrate |
| 3 | **Cap the key frame first.** It is a setting, and at 25 kB the problem disappears even without pacing. | Not built. No direct cap in the tree |
| 4 | **Carry parity on every frame, key frames included, as a percentage of the frame's datagrams.** 20% at the measured loss takes the stream from 1.5% to 99.9% of frames shown, for **less** bandwidth than key-frame-on-demand recovery: a key frame costs eight frames and parity costs a fifth of one. Keep recovery on demand as the backstop. | Built (`Parity`) |
| 5 | **Widen the reorder window to one frame period**, derived from the frame rate in `StreamConfig`. A one-line change, worth 384 frames in 1200 on a link with no loss. | Built |
| 6 | **Do not add slices.** | Held: one slice (H.264), one tile (AV1) |

### What was not measured

The phone was not attached for this part, so nothing here ran against the
Android client. The link data is the laptop's own air hop. A burst and a paced
load did not separate on the real radio: an idle ping to the access point had a
234 ms worst case and a 22 ms mean deviation, which swamps an 8.5 Mbit/s stream.
The case for pacing rests on the shaped link and the buffer mechanism. That
variance was the laptop's radio, not the medium: the phone at the same access
point and load holds a deviation of 1.63 to 2.76 ms (next section).

## The phone was not the throttle. The laptop's radio was

The suspects were 802.11 power save, Doze, App Standby, Samsung Freecess and the
Samsung game manager. All were measured, and none is the cause. The laptop's own
Wi-Fi hop carries 45 times the phone's latency to the same access point, at the
same moment, under the same load. Measured over `adb` on a Galaxy S25
(SM-S931B, Android 16, API 36) attached by USB, and over `ping` on the laptop,
so no measurement used the link under test.

### The two ends were not on the same radio

Both ends associate to the same access point, on different radios:

|        | Radio   | Band            | Rate            | Signal  |
|--------|---------|-----------------|-----------------|---------|
| Phone  | radio A | 5260 MHz, ch 52 | 433/866 Mbps ac | -45 dBm |
| Laptop | radio B | 2462 MHz, ch 11 | 13 to 72 Mb/s   | -35 dBm |

The laptop has the stronger signal and the lower rate, which is a transmit
problem. `iwconfig` reports `Tx-Power=3 dBm` (1 mW) against the normal 20 dBm.
`/sys/module/cfg80211/parameters/ieee80211_regdom` reads `00`, the world
domain. It caps transmit power and marks 5 GHz as no-initiate-radiation. So the
laptop cannot join channel 52 although it sees that radio at full signal. It
falls back to 2.4 GHz at 1 mW, and the access point rate-controls it down. The
power cap and the band are **measured**. The regulatory domain as their cause is
**inferred**: changing it needs root and was not applied, so the configuration
stays as found.

### Subtracting one hop isolates it

Phone to gateway crosses the phone's radio and the access point. Phone to laptop
adds the hop from access point to laptop. At 5 Hz with 56-byte payloads, in
milliseconds:

| Path            | p50    | p90    | p99    | max    | loss |
|-----------------|--------|--------|--------|--------|------|
| phone -> AP     | 4.12   | 12.50  | 51.60  | 51.70  | 0%   |
| phone -> laptop | 52.20  | 329.00 | 416.00 | 437.00 | 12%  |
| laptop -> AP    | 4.98   | 18.70  | 304.00 | 510.00 | 1%   |
| laptop -> phone | 172.00 | 385.00 | 489.00 | 720.00 | 6%   |

The reported symptom was 77 to 94 ms with 3% loss. This baseline is worse, with
the same shape. At about 20 Hz with 1400-byte payloads, the stream's packet size
and rate:

| Path                    | p50    | p90    | p99    | max    | sd     | loss |
|-------------------------|--------|--------|--------|--------|--------|------|
| phone -> AP             | 3.33   | 5.68   | 16.40  | 17.20  | 2.50   | 0%   |
| laptop -> AP            | 151.00 | 355.00 | 403.00 | 405.00 | 135.24 | 0%   |
| laptop -> phone (video) | 11.70  | 139.00 | 270.00 | 373.00 | 60.22  | 0%   |

Phone to access point: 3.33 ms median, standard deviation 2.50 ms, worst 17 ms,
over 240 packets. Three repeats minutes apart gave 3.41, 3.32 and 3.33 ms, with
deviation between 1.63 and 2.76 ms. Laptop to the same access point: 151 ms
median, deviation 135 ms, 45 times the phone. Five consecutive runs gave 148,
134, 134, 152 and 133 ms.

The laptop's hop is bimodal. Its 1.41 ms minimum beats the phone's 1.51 ms, but
most frames retry. That is link-layer retry, not a queue: one 200-packet burst
counted 2346 transmitted packets, 0 transmit errors and 35 excessive retries,
while the bit rate wandered from 13 to 72 Mb/s at a fixed -35 dBm. Video flows
from laptop to phone, straight through that weak transmit. This also explains
the 2.9 ms phone-as-hotspot figure: one hop to a radio in the same room, where
1 mW is enough. The hotspot removed the laptop's bad hop. It did not fix
Android.

### Background traffic is not the cause

Syncthing holds connections on port 22000, one to the phone, and transfers in
bursts. Five identical runs, sorted by the interface byte counters:

| Background tx | p50    | p90    | sd     |
|---------------|--------|--------|--------|
| 5 kB/s        | 152.00 | 357.00 | 129.68 |
| 22 kB/s       | 134.00 | 338.00 | 131.84 |
| 261 kB/s      | 134.00 | 338.00 | 134.67 |
| 269 kB/s      | 133.00 | 351.00 | 138.92 |
| 325 kB/s      | 148.00 | 354.00 | 139.85 |

Sixty times the load does not move the median, and the quietest run is the
worst.

### Every Android suspect, measured and cleared

- **Doze and App Standby are not engaged.** `dumpsys deviceidle` reports
  `mState=ACTIVE mLightState=ACTIVE`, `mScreenOn=true` and `mCharging=true`.
  `am get-standby-bucket com.tachyx.tachyx` returns 10 (ACTIVE, the least
  restricted bucket). The app is not on the battery optimisation allowlist,
  which is correct and costs nothing in the foreground.
- **Freecess does not freeze the app.** `FreecessHandler: skipping freeze
  com.tachyx.tachyx(10558)` is Freecess deciding *against* a freeze. A real
  freeze in the same log: `FreecessController: FZ :
  com.samsung.android.goodlock(10427) reason: Bg`, then `FreecessHandler:
  freeze com.samsung.android.goodlock(10427)`. `tachyx` never takes that path.
  A Samsung-specific false alarm.
- **The game manager is an absent boost, not a throttle.**
  `SGM:SemGameManager: isGamePackage(), ret=false` means no game profile is
  applied, and nothing is removed. Samsung-specific.
- **No thermal throttling.** `dumpsys thermalservice` reports
  `Thermal Status: 0`, with the application processor at 54.2 C and the skin at
  37.7 C after a session. `/proc/pressure/cpu` shows `some avg10=17.22` and
  `full avg10=0.00`.
- **Power save is real, and not in this path.** With the screen off, the phone's
  p90 to the access point rises from 12.50 ms to 41.40 ms, worst 60 ms, at an
  unchanged 5.71 ms median and no loss: a station waking on beacon boundaries.
  With the screen on, where the app streams, it is 3.41 ms median and 13 ms
  worst. The hypothesis had the right mechanism and the wrong case.

### `WIFI_MODE_FULL_LOW_LATENCY` exists here

The device advertises `WIFI_FEATURE_LOW_LATENCY`, and `dumpsys wifi` counts
`322 full low latency` locks taken by other packages. At measurement no lock was
held (`Current operation mode: 0`), and the owner list
`{10551, 10280, 1000, 10411}` did not include `tachyx` (uid 10558). With the
screen on, the phone's hop already ran at about 3.3 ms median with a p99 of 12.5
to 17.4 ms. So a lock can recover at most that tail, 10 to 20 ms, and nothing of
a 355 ms p90 one hop away. It is insurance for screen-off and background scans.
The client now holds the lock while its window is visible
(`app/android/app/src/main/kotlin/com/tachyx/tachyx/MainActivity.kt`).

### What to change

Fix the laptop first. Everything else is worth single-digit milliseconds.

**On the laptop, by hand:** set a real regulatory country instead of `00`, for
normal transmit power and 5 GHz initiation. Then prefer the access point's
5 GHz radio, which is visible at full signal and advertises 1170 Mbit/s. The
expected effect is the gap between the two load-table rows. Not measured: it
needs root and was not applied.

**In the client**, none of it Samsung-specific:

| change | basis | status |
|---|---|---|
| Hold `WifiManager.WIFI_MODE_FULL_LOW_LATENCY` for the session: `createWifiLock(WIFI_MODE_FULL_LOW_LATENCY, tag)`, `acquire()`, release at the end. Needs only `android.permission.WAKE_LOCK`. Documented to apply only with the screen on and the app in the foreground | measured headroom: at most the 12 to 17 ms p99, nothing at the median | done |
| MediaCodec `KEY_PRIORITY` 0 (realtime) and `KEY_OPERATING_RATE` at the stream's frame rate | documented, present on API 36. Not measured, because the decoder was not the limiter | not done. The decoder asks for `low-latency` and vendor keys, and `crates/tachyx-video/src/android/hints.rs` leaves out the operating rate because it crashes several shipping devices |
| `PerformanceHintManager` (API 31 and up) for the decode and render loop, target work duration one frame period | documented. The vendor-neutral way to get the scheduler to favour these threads, in place of the game profile Samsung declined | not done |

Do not add `setSustainedPerformanceMode`: it trades peak clock for a stable one,
and thermal status was 0 throughout. Do not request a battery optimisation
exemption: the app was ACTIVE and not idle in the foreground.

### What was not measured

- The regulatory domain fix, which needs root and was not applied.
  **Inferred:** the laptop's hop would approach the phone's, 3 to 5 ms median
  with single-digit deviation, because the same access point gives that to a
  station it can hear.
- A held low-latency lock, which needed app code that was not yet written. The
  next section covers the lock as built.
- Nothing changed on the phone. The screen was slept and woken for the
  power-save test and left awake.

## The video path stalls, and the cause is a link this system overdrives

The stream runs for a few seconds and then freezes. Host per-tick counters, read
off the quinn connection, show one signature: the UDP datagrams received from
the phone stop for good. With no acknowledgements, the unacknowledged bytes
reach the congestion window and the transport stops. The send buffer fills, the
pre-encode gate refuses, and the pump stops. The gate, the budget and the
watchdog react correctly to a silent peer.

On a good access point (9 ms round trip, two packets lost), the return path died
at about 240 packets a second on the downlink. On a poor one (48 ms round trip,
17% loss), it died sooner, with the window 220 packets into loss.

Halving the frame rate delayed the freeze about fourfold and did not cure it.
That is a queue overflowing, not a fixed rate ceiling. A plain flood reproduces
the cut with no transport involved. **Inferred:** the buffer is most likely in
the phone's wireless driver. A foreground low-latency Wi-Fi lock, held by this
build and confirmed active on the phone, does not prevent it.

The link cannot carry the offer. The offer does not back down, because its
feedback dies with the return path. A window full of bytes that will never be
acknowledged has no way back. The delay-based controller is blind exactly then,
because its reports travel the failed path.

Two changes follow:

1. Start lower, and limit the packet rate as well as the bitrate, because the
   packets are already at the path MTU. Status: sessions start at 2 Mbit/s
   (`START_TARGET` in `crates/tachyx-congestion/src/lib.rs`). No packet-rate
   limit was found in the controller.
2. Reset a dead path: when the return path is silent for over a second with the
   buffer full, drop the stale backlog and start a fresh key frame. Status: the
   host's `Watchdog` (`STALL` of 1 s in `crates/tachyx-hostd/src/session.rs`)
   fires when pictures are offered and none reach the wire. It collapses the
   controller to its 300 kbit/s floor (`MIN_TARGET`), reopens the gate for one
   frame and forces a key frame.
