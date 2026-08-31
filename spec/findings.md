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
Resolution and frame rate changes are made by pushing new caps at the source; the
scaler renegotiates and the encoder reconfigures in place, costing one keyframe
and one frame of delay. Driving a resolution change from a downstream filter
instead stalls the pipeline, so it must come from the source.

A forced keyframe takes effect on the very next frame, and re-sends the parameter
sets with it.

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
pressure quinn discards queued datagrams to make room, and it discards arbitrary
ones, which corrupts frames rather than skipping them. In the burst test 2166 of
5000 vanished with no error reported. The send buffer is therefore sized to about
two frames and the available space is checked before a frame is queued, so that a
frame is dropped whole, by us, rather than shredded by the transport.

Datagram frames are written into a packet before stream frames, so control traffic
cannot starve video within a packet, though the two still share the window.

The default transport settings are wrong for a local network by large factors. The
initial round trip estimate defaults to 333 ms, which seeds loss detection, so a
lost packet is not suspected for a third of a second on a link where the real
figure is under one millisecond.

quinn defaults to one cryptographic provider and rustls to another, so accepting
both defaults compiles both. The provider is selected explicitly.

Recovery is by retransmission rather than forward error correction. Sunshine and
Moonlight carry a fifth of their bandwidth as parity because a wide-area round trip
exceeds a frame period, making retransmission useless. On a local network a
retransmission costs about a millisecond, so parity on every frame would be paying
continuously for a problem that can be fixed on demand. The header reserves room
for parity for when this leaves the local network.

## Discovery

The host advertises over mDNS. On Android the client uses the platform's own
service discovery, which acquires the multicast lock itself for foreground
applications, so the application needs no permission for it. Requesting the
multicast permission directly would add an implied hardware requirement that
filters the application out of the store listing for devices without Wi-Fi, and a
held lock keeps the radio waking on every beacon interval.
