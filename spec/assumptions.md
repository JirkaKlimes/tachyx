# What this build assumes

This project has run on one setup. A NixOS laptop with Hyprland, an AMD Radeon
780M, two outputs at 2880x1800 and 3440x1440, talking to a Galaxy S25 over a
home Wi-Fi network. Nearly every number in the code was measured on that pair,
and most of them were measured on one day. [`findings.md`](findings.md) and
[`congestion.md`](congestion.md) are honest about which figures were run and
which were reasoned, and they are the reason this audit was possible at all.

This document asks a different question. Not *is the number right here*, but
*what happens when this meets a machine that is not this one*. A different
compositor, a different GPU, a different phone, an iPhone, a slow link. It is
ordered by what the fix costs, not by subsystem, so it can be read as a work
queue.

Three words are used the way the other spec documents use them. **Read** means
the claim comes from the source and nothing was run. **Measured** means a figure
in the repository that was taken on the target machine. **Inferred** means a
conclusion drawn from reading plus arithmetic, with no run behind it. Where this
document is guessing it says so, and the last section collects the guesses.

Nothing here was executed. Every location was read.

---

## What breaks on the next machine

These are cheap. Most are one line, and each one is between somebody else and a
picture on their phone.

### The daemon cannot start without a configuration file, and the file it needs names this laptop's monitors

[`config.rs:29-32`](../crates/tachyx-hostd/src/config.rs) documents `output` as
optional and says that "unset means the first output the compositor reports".
Nothing implements that. Two different places resolve the same unset field, and
they disagree with the documentation and with each other.

`session.rs:117` passes `config.output.as_deref().unwrap_or_default()` to
`Capture::start`, which is the empty string. `State::find_output` in
`capture.rs:1085` compares that against every `wl_output` name by equality, and
no compositor announces an output called nothing, so it returns
`Error::NoSuchOutput("")`. The user sees `no output named` with nothing after
it.

`main.rs:55` resolves the same unset field to `"eDP-1"`, which is the internal
panel of the machine this was written on. On a desktop whose outputs are `DP-1`
and `HDMI-A-1` the daemon refuses to start with `no output named eDP-1`, and it
does so before it binds the socket, before capture is ever reached.

So: a host with no configuration file cannot serve, the error names a monitor
that has nothing to do with the real fault, and the two halves of the daemon
would capture and inject on different screens if it ever got that far. The fix
is to pick the first output the compositor reports, once, in one place, and pass
it to both.

### A compositor that is not wlroots fails with the wrong error first

Capture requires `ext-image-copy-capture-v1` and injection requires the wlroots
virtual pointer. GNOME has neither. KDE has the capture protocol on recent
versions and has never had the wlroots pointer. There is no
`zwlr-screencopy-v1` fallback anywhere in the tree, despite
[`findings.md`](findings.md) measuring both, and there is no portal path,
which was cut deliberately.

The protocols are all checked and all named in their errors, which is right.
`Error::Missing(&'static str)` in `media/src/lib.rs:56` and
`input/src/lib.rs:46` produce sentences a person can act on. But the order is
wrong. `main.rs:55` builds the injector before anything else, and
`Injector::new` checks the xdg output manager, then the *named output*, then the
virtual pointer. So on GNOME or KDE the first thing a new user reads is `no
output named eDP-1`, which points at the configuration file when the real answer
is that the whole protocol family is absent. Setting `output` in the config
advances them one line, to
`compositor does not offer zwlr_virtual_pointer_manager_v1`.

Checking for the protocols before resolving a name would turn two confusing
attempts into one clear refusal. That is the cheap half. The expensive half is
below.

There is a smaller version of the same problem in `capture.rs:475` and `:478`,
which bind the two ext protocols with the advertised version rather than
`version.min(1)` as every other bind in the tree does. Both protocols are at
version 1 upstream today, so this is latent. A compositor advertising version 2
would hand back a proxy whose events the generated code cannot decode.
**Inferred:** that surfaces as a protocol error and a `DispatchError`, not as a
clear message.

### The key frame interval is counted against a frame rate the encoder is not running at

`encode.rs:756` sets `key-int-max` to `KEY_FRAME_INTERVAL_SECONDS * 60`. The
constant is documented at `encode.rs:54-60` as "how long a lost frame may leave
the picture wrong", which is a time, and the property is a frame count. The 60
that converts one into the other is not this machine's refresh rate, which is 75
or 120, and it is not the declared rate either, which the ladder moves between
75, 37.5, 25, 18.75 and 15 frames a second.

So the two seconds the comment promises is 1.6 seconds at full rate on DP-2, and
eight seconds at the ladder's slowest rung, which is exactly the moment the link
is bad and a stale picture costs most. The property is also set once, in
`configure_encoder`, and never rewritten when the ladder changes the declared
rate.

The value should be `KEY_FRAME_INTERVAL_SECONDS * declared_fps`, applied
alongside the rate in `apply_rate`. The declared rate is already in hand there.

### The dead host threshold is sixteen heartbeats, and four places say three

`client/session.rs:69` sets `LOST_AFTER` to `HEARTBEAT_INTERVAL * 16`, which is
four seconds, and it gives a good reason: three missed beats fired on Wi-Fi
while video was still arriving, because a heartbeat is a stream frame and quinn
writes datagrams into a packet ahead of stream frames.

Four other places still describe the old value. `client/session.rs:113` says
"three heartbeat intervals". `client/session.rs:158` says "lost three heartbeats
after the connection opened". `client-ffi/api/session.rs:47` repeats the
sentence verbatim. `proto/control.rs:348` says "three missed beats are then
noticed three quarters of a second in", which is the paragraph that justifies
`HEARTBEAT_INTERVAL` itself. The test at `session.rs:1115` only asserts that two
beats is still `Idle`, so nothing catches the drift.

This is a documentation fix, but it is the kind that matters, because the next
person tuning the heartbeat will read `control.rs:348` and reason from a number
that has not been true for a while.

### The cursor is polled at sixty hertz on a machine that has no sixty hertz output

`hostd/cursor.rs:24` sets `INTERVAL` to 16 ms and calls it "one video frame at
sixty hertz". Neither output on the development machine is 60 Hz. The client's
`BLEND` of 0.35 at `client/cursor.rs:38-41` is justified as clearing an error
"inside a tenth of a second" *at sixty reports a second*, so the client's
smoothing time constant is a function of a host-side constant that neither file
names.

Both are defensible as fixed rates. What is not defensible is that they are
coupled and neither says so. Either derive the cursor interval from the capture
refresh, or state in both files that 16 ms is a wire fact the two ends share, the
way `HEARTBEAT_INTERVAL` is stated.

### Keeping the screen awake is a hard requirement, and its mechanism is one compositor's

`session.rs:92` does `let _awake = crate::idle::Inhibitor::new()?;` before the
pipeline starts. A compositor without `zwp_idle_inhibit_manager_v1` cannot serve
a session at all, even though nothing about the picture depends on it. That is a
convenience promoted to a blocker by a question mark.

Worse, the mechanism is specific. `idle.rs:31-42` explains that the inhibitor
sits behind a surface that is never committed and never given a role, and that
"a compositor cannot decide whether such a surface is visible, and this one
resolves that by treating it as visible". That is a statement about Hyprland.
**Inferred:** on a compositor that treats an uncommitted roleless surface as not
visible, the inhibitor is created, no error is raised, and the screen blanks
mid-session. That is the silent version of this failure and it is worse than the
loud one.

Log a warning and carry on when the protocol is missing. Note in the module that
the visibility rule is one compositor's answer and is not in the protocol.

### The second client is not refused, it hangs

`main.rs:73-90` awaits `session::serve` inside the accept loop, and `serve`
takes `injector: &mut`. One session at a time is therefore structural, which is
fine for this milestone and is what [`acceptance.md`](acceptance.md) says.

The problem is what a second client experiences. `endpoint.accept()` is not
polled while a session runs, so the second connection is never completed and the
second client sits in its own handshake until it times out. It is told nothing.
A `Refused` close with a reason costs one `accept` in a `select!` and turns a
mystery into a sentence.

### A client that leaves mid-gesture leaves the button down

`Injector` holds `held` and `locked` at `input/lib.rs:127-128` and there is no
path anywhere that releases them. The injector outlives the session, because
`main.rs` holds one for the daemon's life and lends it to each `serve`.

So a client that disconnects while dragging, or with Shift down, leaves the
host's compositor with a button held and a modifier latched, for ever, with no
local input able to clear it short of pressing the real key. The client tracks
its own held keys at `keyboard.dart:38` but only to release them on
backgrounding, and a lost connection is not backgrounding.

A `release_all` on session end is a dozen lines and it is the difference between
a dropped connection and a machine somebody has to go and rescue.

### The transport's send buffer is sized for a phone, and a desktop client gets no picture at all

`main.rs:51` sizes the transport once, before any client has said anything, for
`CodedSize::new(1920, 1088)`. The comment at `main.rs:42-50` is careful and
correct about why a generous buffer is not free. Its premise is that "1080p is
the largest a phone asks for".

The ladder's top rung is not bounded by that. `session.rs:145` builds the ladder
from `top_rung(capture_size, viewport(...))`, and `viewport` is the client's own
surface. A Linux client with a 2560x1440 window against this 3440x1440 host gets
a top rung of 3440x1440.

The arithmetic, **inferred** rather than run. `video_send_buffer` for 1920x1088
comes to 303,778 bytes. The gate at `video.rs:378` charges a frame its payload
plus 16 bytes of header plus 32 bytes of quinn's own accounting per datagram,
and it charges the key frame's parity with it. A key frame of 250 kB at
3440x1440 needs 334 kB and is refused. Every time.

The watchdog makes it worse rather than better. `session.rs:378-403` notices
that nothing is reaching the wire, opens the gate for one frame and forces a key
frame. `VideoSender` then refuses that frame on its own check, the frame keeps
its identifier, the client reads the hole as a break in the reference chain and
asks for another key frame. The loop runs once a second, logs a warning that
does not name the cause, and never produces a picture.

The measured key frame at 3440x1440 is 192 kB, which does fit, so this is a
content-dependent cliff rather than a certainty. That is the worst shape for a
bug. Size the buffer from the coded size the session actually chose, which is
known by the time `stream_config` is sent, or refuse a top rung the buffer
cannot hold and say so.

### Two address parsers in Dart cannot read an IPv6 literal

`app.dart:45-53` and `connect_screen.dart:73-82` are the same eight lines
written twice: `lastIndexOf(':')`, `int.tryParse`, fall back to
`HostLink.defaultPort`. Given `[fe80::1]` the last colon is inside the brackets,
so the host becomes `[fe80:` and the dialled string is `[fe80::47989`.

The Rust side already does this properly, in one place, with the brackets
stripped: `driver/src/lib.rs:303-308`. The Dart copies should call into it or be
replaced by one function that does what the Rust one does.

### The development shell only works on NixOS, and takes over the shell

`flake.nix:143` and `:144` put `/run/opengl-driver/lib` on `LD_LIBRARY_PATH` and
`/run/opengl-driver/lib/dri` in `LIBVA_DRIVERS_PATH`. Those paths exist on NixOS
and on no other distribution. The README points at `nix develop` as the way to
get a toolchain, so on Debian or Fedora with Nix installed the shell builds and
the VA-API driver is not found.

`flake.nix:168` runs `exec zsh` for any interactive shell. That is one
developer's shell imposed on everyone who enters the environment.

Neither is hard to fix. Fall back to the system paths when
`/run/opengl-driver` is absent, and let the shell be whatever started it.

### Smaller things, listed rather than argued

`connect_screen.dart:127` uses `192.168.1.20` as the host field placeholder,
which is one person's LAN.

`endpoint.rs:60` sets `initial_mtu` to 1472 with the comment "a 1500-byte path
less twenty bytes of IPv4 and eight of UDP". Over IPv6 the header is forty
bytes, so the correct figure is 1452. **Inferred:** quinn's path discovery
lowers it, so the cost is a handful of dropped packets at the start of an IPv6
session rather than a failure.

`configure_encoder` at `encode.rs:734-768` writes nine properties with
`set_property` and `set_property_from_str`, neither of which returns an error.
**Read from the glib API contract, not run:** an unknown property name panics.
`make()` at `encode.rs:728` already models the graceful case for element names.
Properties have no equivalent, so any element substitution that renames one
crashes the encode thread rather than producing `Error::MissingElement`.

`stats_overlay.dart:185-192` cannot show `parity_received` or
`frames_recovered`, because `client-ffi/api/session.rs:67-83` does not carry
them across the FFI even though `ReassemblyStats` at `packet.rs:619` and `:624`
counts both. Forward error correction is the newest and least proven mechanism
in the system, and the overlay built to observe the system cannot see whether it
is working.

`touchpad.dart:325-330` sends the raw finger delta positive-down,
`proto/input.rs:99-102` declares positive-down, and `input/lib.rs:283-305` feeds
it to `wl_pointer::axis` unchanged. So two fingers down scrolls content up,
which is the opposite of what every phone does, and there is no setting.

`keyboard.dart:61` and `input/lib.rs:310-315` both drop anything that is not HID
page `0x0007`, so consumer page `0x000C` is discarded twice: no volume, no
brightness, no media transport, no browser keys, ever.

---

## What breaks later, because the shape is wrong

These are not one-line fixes. Each one is a decision that is currently made in
the wrong place, and each gets more expensive with every week of code written on
top of it.

### The encoder is not one element name away from anything

[`findings.md:187`](findings.md) says NVENC "stays available as a second
backend, one element name away". That is not true of this code, and the reason
matters more than the count.

There are two element names, not one: `vapostproc` at `encode.rs:567` and
`vah264enc` at `:574`. The caps feature between them, `memory:VAMemory` at
`:645`, is VA-API's and is described in its own doc comment as load-bearing.
Seven of the nine properties are VA-API's: `target-percentage` at `:748` and
`mbbrc` at `:763` have no NVENC analogue at all, `cpb-size` at `:664` is
`vbv-buffer-size` there, `rate-control` at `:739` and `key-int-max` at `:756`
are spelled differently, and as noted above a wrong property name panics rather
than erroring.

The deepest coupling is the one that crosses a file boundary and is easy to
miss. `import_layouts()` at `encode.rs:775` opens the `vapostproc` factory and
harvests the DRM formats from its DMA-BUF sink templates.
`choose_layout()` at `capture.rs:1112` calls it, and `capture.rs:1126-1134`
demands an exact format and modifier present in both the compositor's list and
that element's. So **capture's allocation format is a function of which encoder
plugin is installed**, and with no VA plugin the failure is raised from inside
capture. That is not a swap, it is a redesign of the negotiation.

Two more consequences. The buffer handed to GStreamer at `encode.rs:669-705` is
a DMA-BUF descriptor with a `GstVideoMeta`; **inferred**, the nvcodec elements
do not import DMA-BUF, so an NVENC path needs an upload, which is the PCI
Express copy [`findings.md:184`](findings.md) says it wants to avoid. And
`media/lib.rs:188`, `:190` and `:320` state `vah264enc`'s pad template limits
and its property maximum as if they were H.264's.

What this asks for is a backend that owns four things: the element names, the
memory feature string, the property map, and the importable-layout query. Then
capture stops naming a VA element at one remove. Intel is nearly free once that
exists, because it is the same two elements. NVIDIA is real work whatever
happens, and it is a lot less work behind a seam than in front of one.

The same shape holds for a second codec. `encode.rs:42`, `:943` and `:937-976`
are H.264 Annex B parsing, `media/lib.rs:253` rounds to the H.264 macroblock,
and `Codec` at `proto/control.rs:104` has one variant.

### Capture and injection have no seam at all

The second compositor is the bigger version of the same problem, and it is
larger than the encoder because there is nothing to put behind a trait yet.

Injection is not a set of globals to swap. `Injector::apply` and everything
under it at `input/lib.rs:212-363` speaks Wayland requests directly, and
`keymap.rs` exists entirely to synthesise an XKB keymap so that a phone's
on-screen keyboard can produce a character no key on the host makes. The portal
RemoteDesktop interface takes keysyms, not keymaps, so that whole file has no
counterpart on the other side. It is a different transport, not a different
name.

Capture is the same. `Pump::setup` at `capture.rs:747-852` and the dispatch
impls at `:518-606` are the protocol. A portal and PipeWire path replaces all of
it, and `PointerCapture` at `:1230-1352` has no portal equivalent at all,
because the portal paints the cursor into the frame. That would take the
client-side pointer away, which is the thing the whole input design is built on
(see [`protocol.md`](protocol.md), "The cursor").

None of this needs building now. What it needs is for `hostd` to depend on a
capture trait and an injection trait rather than on `tachyx-media::Capture` and
`tachyx_input::Injector` concretely, so that the second implementation is an
addition rather than a rewrite of the session loop. That is cheap today and gets
dearer every time the session grows another call into either.

### The measured key frame is a law in four places

`130_427` bytes is one 1080p key frame out of one encoder on one machine. It
appears as `MEASURED_KEY_FRAME_BYTES` at `transport/video.rs:64`, scaled by
pixel count against `MEASURED_KEY_FRAME_PIXELS` at `:67`, and again as a flat
`130_000` at `hostd/idr.rs:83`, plus twice more in tests. From it are derived
the transport's send and receive buffer size, the pacer's floor, and the key
frame budget's opening interval.

The pixel-count scaling is the part to be suspicious of. A key frame's size is
set by the picture's detail and the encoder's quantiser, not by its area, and
[`congestion.md`](congestion.md) measured that same encoder spanning two orders
of magnitude in each direction against its configured bitrate on different
content. Scaling by pixels is a defensible shape for a *buffer bound*, which is
what it is used for. It is not defensible as an estimate, and `idr.rs` uses it
as one until the first key frame lands.

The right answer already exists in the code and is only half wired.
`VideoSender::key_frame_estimate` follows the measured size, and `answer()` at
`session.rs:618` feeds the measured size into the budget before every request.
What remains is the startup case and the buffer sizing, and both should come
from the coded size the session chose rather than from a constant. The
`Tuning::for_stream` call in `main.rs:63` happens before a client exists, which
is the thing that has to change.

### The client cannot say what it can decode

`Capabilities` at `proto/control.rs:109-122` carries a viewport, a maximum
decode size, two frame rates and a bitrate ceiling. That is all.

[`protocol.md`](protocol.md) describes `ClientHello` as carrying "decoders the
client can drive (codec, profile, maximum level, resolution and frame rate, bit
depth, chroma subsampling), the viewport in physical pixels, the device pixel
ratio, and the requested resolution mode". Almost none of that exists.

Worse, `max_decode` is not a decoder capability. `client/session.rs:90` sets it
to the viewport, with a good argument at `:81-87`: a picture larger than the
surface is scaled down before anyone sees it. That argument is about *waste*, and
the field is used by the host as a *limit*. The client never asks the platform
decoder what it can do.

Today that works because the one client is a phone whose decoder comfortably
exceeds its own panel. It stops working at the first device where those two
differ, and it has no answer at all for a client that cannot do High profile, or
that wants H.265, or that is a browser. Adding the fields is easy now, while the
protocol version is 1 and there is one client. It is a migration once there are
three.

The mirror of this is that the host announces nothing. `HostHello` at
`control.rs:190` carries a version and, as its own comment says, "at draft 0
nothing else". No encoder list, no monitor list, no capture formats. A client
cannot choose a monitor because it cannot be told there is more than one, and
`client/cursor.rs:216-223` treats "the pointer is on another monitor" as
"draw nothing", which is the only multi-monitor behaviour in the system.

### The Android decode path holds a lot of code that is not Android's

This is the specific worry that prompted the audit, and the answer is not where
it was expected.

**The Kotlin is fine.** Two files, 162 lines, about 82 of them code, and roughly
60 of those are mechanism only the JVM can perform: `TextureRegistry`,
`createSurfaceProducer`, the `SurfaceProducer.Callback` interface,
`System.loadLibrary`, and the activity class the manifest names. There is no
codec string, no colour format, no bitstream handling. `handlesCropAndRotation()`
at `VideoSurface.kt:82` is reported to Dart rather than acted on, which is the
right split.

The three things worth moving out of Kotlin are small. `VideoSurface.kt:51`
silently defaults a missing width or height to `1`, which produces a one-pixel
image reader and a black screen with no error, and Dart already holds the typed
value. `VideoSurface.kt:94` repeats an idempotence check that
`session_screen.dart:218-222` has already done. And the `open`/`resize`/`close`
verb set with its stringly-typed `id`/`width`/`height`/`appliesCrop` keys is a
hand-rolled RPC standing next to a typed FFI that already exists in the same
application.

**The real cost is in Rust.** `crates/tachyx-video/src/android/bitstream.rs` is
459 lines of pure H.264 arithmetic: an Annex B walker, an SPS parser, an
emulation-prevention-aware bit reader. Nothing in it touches a platform call, and
`lib.rs:46-48` says so and compiles it everywhere. It is `pub(super)`, inside a
module named `android`. Any second decoder needs exactly this code, because
VideoToolbox builds its format description from SPS and PPS and Media Foundation
wants the same parse.

`android/present.rs` is about 490 lines and only `android/decoder.rs` under it
is genuinely MediaCodec. Queue depth and the drop-to-resync rule at `:14-21` and
`:150-158`, key frame gating at `:296-304`, the stall reason strings, the
latency accounting at `:313-324`, and the surface-generation comparison at
`:455-469` are all policy that any decode loop wants.

Moving `bitstream.rs` out of `android/` is a rename today. Splitting the policy
half of `present.rs` out is an afternoon. Both get harder as the module grows,
and both are the difference between an iOS client that writes a decoder and one
that writes a decode subsystem.

`tachyx-surface` has the right shape and the wrong types. `Producer`, `Surface`
and the generation counter are exactly the abstraction a second platform wants,
and they are concrete types behind `#[cfg(target_os = "android")]` with no trait
to implement.

### The bits per pixel figure is one measurement divided by one resolution

`ladder.rs:32` sets `OPENING_QUALITY` to 0.06 and documents where it came from:
a busy quarter-screen window at 1080p60 carrying 7.86 Mbit/s, which is 0.063
bits per pixel per frame. `worst_case` at `ladder.rs:398` then applies it at
every rung, multiplied by that rung's pixel count.

Bits per pixel is not resolution-invariant for desktop content. The same window
at twice the linear size carries the same detail over four times the pixels, so
the cost per pixel falls. **Inferred:** this over-estimates on large captures,
so a session on a 4K host opens lower than it needs to, and under-estimates on
small ones. It only chooses the opening point, and the ladder closes the loop on
real production after the first second, so the consequence is bounded. But it is
the one number that decides what the user sees first, and it was taken at one
resolution on one encoder.

Two more constants in the same file are stated against one monitor.
`MAX_STRIDE = 5` at `:38` is justified as "15 frames a second on a 75 Hz
output", but it is a stride, so the floor it produces is 24 frames a second on a
120 Hz panel, where the ladder then has no rung left for a bad link, and 6 on a
30 Hz one, which is well below the "stops feeling connected" line the constant
exists to hold. `MIN_TARGET` at `congestion/lib.rs:49` is 300 kbit/s because
"the smallest rung of the ladder, 860x360" needs 279 kbit/s, and 860x360 is
3440x1440 divided by four.

All three should be derived: the stride floor from a target frame rate, the rate
floor from the smallest rung this capture actually has.

### Nothing reads a display transform, a fractional scale, or a colour space

There is no handling of `wl_output` transform anywhere in the tree. A rotated
monitor is captured in its transformed buffer size and injected against its
logical extents, and nothing reconciles the two.

Scale is subtler and worth stating carefully. `input/lib.rs:437-452` computes
the layout bounding box from xdg-output, which is *logical* pixels, and it
correctly does not assume the origin is at zero. The cursor feed reports the
captured screen in *device* pixels. Input positions travel as fractions, so the
scale cancels today, because both fractions come off the capture side.
**Inferred:** a fractionally scaled output puts the injector and the cursor feed
in different spaces, and nothing checks.

Colour is simply absent. `capture.rs:1116-1121` will negotiate exactly four
formats, all 8-bit RGB, so a 10-bit output fails with `Error::NoSharedFormat`,
which is loud but says nothing useful. `StreamConfig` at `session.rs:730-752`
carries codec, sizes, aspect, frame rate and bitrate, and no transfer function,
no primaries, no range. An HDR or wide-gamut desktop that does negotiate is
encoded and sent unlabelled, and the client draws it as if it were sRGB.

None of this needs solving now. What it needs is for `CaptureFormat` and
`StreamConfig` to have somewhere to put the answer, so that adding it later is a
field rather than a protocol version.

### The frame deadline and the reorder window cross over at the ladder's own floor

`client/session.rs:29-31` states the invariant: the 50 ms `FRAME_DEADLINE` is a
fallback for the frame nothing follows, and "every other case is decided sooner,
by the reassembler's own reorder window". The window is set at `:253` to one
frame period, which is the right derivation and is exactly what
[`findings.md`](findings.md) recommends.

One frame period is under 50 ms only while the frame rate is above 20. The
ladder's own floor is 15 frames a second, which is a 66.7 ms window against a
50 ms deadline. At the slowest rung the fallback fires first and the stated
invariant is inverted, on precisely the link that drove the ladder down there.

The deadline should be derived from the same frame rate, or be the larger of the
two.

A second coupling of the same kind sits across a crate boundary that cannot be
crossed. `pace.rs:62` sets `FRAME_SPAN` to 35 ms because "the receiver gives a
frame 50 ms from its first datagram and then abandons it". That 50 is
`client/session.rs:32`, and `tachyx-transport` cannot depend on
`tachyx-client`. Today it is a number retyped in a comment. It belongs in
`tachyx-proto` next to `HEARTBEAT_INTERVAL`, which solved the identical problem
by putting the shared wire fact in one place and saying why.

### Facts written twice

The port was one of these and is now cross-referenced in both directions
(`config.rs:14-15` and `link.dart:79-81`), which is the pattern the rest should
follow. There is a third uncommented copy at `driver/examples/connect.rs:36`.

The ones that still need two edits, ordered by what a wrong edit costs:

**The coded picture size is derived twice from independent sources and never
checked.** The host computes it at `session.rs:735` and sends it, and it reaches
`VideoSurface.kt:92` through Dart to size the image reader. The decoder
independently parses it out of the bitstream at `android/bitstream.rs:59-112`
and hands that to MediaCodec at `android/decoder.rs:53`. If the two ever
disagree the compositor reads a buffer of the wrong shape, which is the hazard
`session_screen.dart:210-214` warns about, and nothing asserts they match.

**The JNI symbol names encode the Kotlin package in five places**: defined at
`surface/android.rs:100`, `:126`, `:146`, re-listed at `surface/lib.rs:31-35`,
and matched by `VideoSurface.kt:1` and `:117-129`, against the Gradle
`namespace` and `applicationId` at `app/build.gradle.kts:8` and `:19`. Renaming
the application id breaks the link at run time with no build error.

**Annex B parsing exists three times**: `android/bitstream.rs:153-188` in Rust,
`probe_video.dart:185-214` in Dart, and `h264parse` on the Linux path. The NAL
type constants and the `& 0x1f` mask are written out in the first two.

**The HID keyboard page** is `HidUsage::KEYBOARD_PAGE` at `proto/input.rs:52`
and `keyboardPage` at `keyboard.dart:14`, because the frb mirror at
`client-ffi/api/input.rs:38-46` does not carry the associated constant across.

**The method channel contract** is `'tachyx/video'` plus four argument keys and
three method names, written at `present.dart:13`, `:89-124` and at
`VideoSurface.kt:26`, `:36-51`, with no "must match" note on either side.

**Three Rust target lists disagree.** `rust-toolchain.toml:8-17` and
`client-ffi/rust-toolchain.toml:10-22` both name `armv7-linux-androideabi`;
`flake.nix:49-52` composes only `arm64-v8a` and `x86_64`. Each file's comment
tells the reader to keep the others in step, which is the tell.

**The 5 second transport idle timeout** is set at `endpoint.rs:58` and restated
in prose at `driver/lib.rs:41` and `proto/control.rs:349`. **The 1.53 second
capture idle gap** appears at `client/session.rs:56`, `:102`, `:970` and
`client-ffi/api/session.rs:37`. **Macroblock alignment 16** appears at
`control.rs:56`, `media/lib.rs:253`, `bitstream.rs:109` and
`video_texture_view.dart:14-18`.

And one that is not duplication but is worth catching here. `control.rs:43-49`
says every ladder rung is a multiple of sixteen, "which avoids the problem
rather than detecting it". That is not what the code does. `EncodeSize::fit` at
`media/lib.rs:225-235` rounds the *visible* size to even, and says so in its own
inline comment, contradicting its own doc line at `:216`. `coded()` at `:253`
then rounds up to sixteen and the difference is a real crop, which the client
detects and corrects with `VideoCrop` in `video_texture_view.dart:25-61`. The
test vector at `bitstream.rs:298` records a live 2580x1080 visible against
2592x1088 coded. So the `CodedSize` constraint never rejects anything, the crop
is real, and the design note attached to the type describes a system that is not
this one. That matters because the next reader may believe the crop handling is
redundant and delete it.

---

## Defaults that are fine, and should say so

Not everything measured here is a problem. A number can be a perfectly good
default and still deserve a line saying it is one, so that the next person knows
they are allowed to change it.

[`congestion.md`](congestion.md) already does this properly, in its
"Confidence" section, and it should be credited rather than repeated. It names
the 2.0 Mbit/s start, the 150 kbit/s control reserve, the 100 ms bucket ceiling,
the 0.8 and 1.3 retarget band, the 10% and 60% ladder thresholds and the 15
frames a second floor as guesses, and it says that every constant in the
estimator and the probe controller is libwebrtc's rather than tuned here. That
is the model. Nothing below needs more than the same treatment.

The GCC constants in `congestion/delay.rs`, `rate.rs`, `loss.rs` and `probe.rs`
are libwebrtc's published values. They are known to work on paths far worse than
this one and none has been tuned here. `delay.rs:70`, the 6 ms threshold floor,
is the one `congestion.md` flags as most likely wrong on Wi-Fi, and that
judgement stands.

`PACE_PERCENT = 250` at `pace.rs:49` is libwebrtc's `kDefaultPaceMultiplier`.
Its own doc explains why it cannot be lower, from the encoder's measured
overshoot on this hardware, and why it is not higher. `MAX_BURST = 25_000` at
`:72` and `BURST_SPAN = 15 ms` at `:86` are both measured here and both have the
mechanism written down. `FRAME_SPAN = 35 ms` at `:62` is the one to revisit,
for the reason in the section above.

`Parity::FLOOR = 5`, `CEILING = 50` and `MEASURED = 20` at `packet.rs:170-180`
are a sensible clamp around one Monte Carlo. The key frame boost of half again
at `:223` is explicitly the lower half of libwebrtc's, with the reason given.
The percentage-of-datagrams shape is right and is the part that generalises;
the level is a starting point that the receiver report is meant to drive and
does not yet.

`REORDER_WINDOW = 15 ms` at `packet.rs:603` is already documented as a floor
under a value derived from the frame rate, which is exactly the treatment every
other measured constant wants.

`Budget::SHARE = 0.15` at `idr.rs:71` and `MAX_INTERVAL = 4 s` at `:74`,
`RING = 3` at `capture.rs:49`, `QUEUE_DEPTH = 8` at `android/present.rs:21`,
`BUCKET_CEILING = 100 ms` at `encode.rs:52`, `INPUT_TICK = 4 ms` at
`driver/run.rs:27`, `SNAP = 0.05` and `BLEND = 0.35` in `client/cursor.rs`,
`holdDelay = 500 ms` at `touchpad.dart:119`: all reasonable, none of them laws.
Several carry a 60 Hz justification for a value that is fine at other rates
anyway, and saying so is cheaper than deriving them.

Port 47989 is arbitrary and unregistered. It is fine, it is cross-referenced,
and the only thing missing is a line saying it was chosen rather than assigned.
The 20 Mbit/s client ceiling at `client/session.rs:77` and the 40 Mbit/s host
ceiling at `config.rs:52` are both ceilings on a controller that works below
them, which is the right relationship.

`WHEEL_DETENT = 15.0` at `input/lib.rs:367` is the Wayland convention for one
notch, not a measurement.

---

## What a second client costs today

### iOS

There is no `ios/` directory. `app/` holds `android/` and `linux/` and
nothing else, and `flake.nix:17-21` targets two Linux systems, so a build host
does not exist in this repository either.

`present.dart:34-36` dispatches on `Platform.isAndroid` alone, so iOS falls into
`_Registered`, which calls `session.presentOn`, which resolves to
`tachyx_video::unsupported::VideoTexture` and returns `WrongTarget`. An iOS
build compiles, links, runs, connects, and shows no picture. That is the
starting position.

The unavoidable work is real and none of it is avoidable by moving code:

The Xcode target, `Info.plist`, signing and an `AppDelegate` that does what
`MainActivity.kt:27-31` does. A `NSLocalNetworkUsageDescription`, without which
iOS 14 and later blocks a connection to a private address outright, which the
Android manifest needs no counterpart to. `VTDecompressionSessionCreate` and
`CMVideoFormatDescriptionCreateFromH264ParameterSets`. Implementing the
`FlutterTexture` protocol and registering with `FlutterTextureRegistry`, which
matches neither the Linux `FlTextureGL` path nor the Android surface producer.
Annex B to AVCC conversion, which has no counterpart anywhere in this repository
because the host emits byte-stream format and both current clients take it
untouched. `kVTDecompressionPropertyKey_RealTime` and its neighbours, which are
a different vocabulary from `hints.rs`. And a frame lifetime discipline across
the pull callback, which is a third answer to a problem Linux solved with
`HELD_FRAMES = 3` and Android avoids entirely.

The architectural difference under all of that is worth stating plainly. The
Android design is "the decoder writes into the compositor's buffer and nothing
we own ever sees a frame" (`android/mod.rs:3-8`). iOS has no equivalent.
VideoToolbox hands back a `CVPixelBuffer` through a callback. So the central
claim of the client's decode path does not transfer, and the iOS path will look
more like the Linux one than the Android one.

The work that exists only because of where code sits today:

The `tachyx/video` method channel written a third time in Swift, including its
silent defaults. A third `Presentation` variant, and with it a third meaning for
the `engine_handle: i64` parameter, which already means an irondash engine
handle on Linux and a Flutter texture id on Android while
`client-ffi/api/session.rs:257` and `api/video.rs:120` still document only the
first. That is an integer where a sum type belongs, and the third platform is
what makes it untenable. The H.264 parser reached through `mod android` or
copied a fourth time. And the whole of `present.rs`'s policy half rewritten
around a callback-driven decoder.

Everything below the presentation layer is free. `tachyx-client`,
`tachyx-driver` and `tachyx-transport` depend on quinn, rustls with ring, tokio,
postcard and serde, and nothing platform-specific. The ring choice at
`endpoint.rs:82-85` was made for Android and helps here too. The entire Dart
input layer already handles `PointerDeviceKind.mouse` beside touch, and
`VideoCrop` is pure Dart with its own tests.

Two Android-shaped things in Dart would need attention rather than
reimplementation. `keyboard.dart:22-28` returns true from the key handler with
an Android back-gesture justification, and there is no `PopScope` anywhere, so
the mechanism is Android-only and incomplete even there.
`keyboard.dart:207` sets `enableIMEPersonalizedLearning`, which is Android-only
and is the thing protecting a typed password from the keyboard's learning store;
on iOS that protection silently does not exist. And **inferred, not read**:
`keyboard.dart:151-157` detects backspace by watching for a deletion against an
empty document, which UIKit does not generally report, so backspace may never
reach the host on iOS. That one wants a test before anybody claims support.

### Windows

There is no `windows/` directory. Windows compiles, links, and fails at run time
in `unsupported.rs:12-13` with a good message and no compile-time signal.

Unavoidable: the runner scaffolding, a Media Foundation or D3D11VA decoder, and
a `FlutterDesktopGpuSurfaceDescriptor` texture over a shared `ID3D11Texture2D`.
`tachyx-glctx` produces nothing off Linux and `TEXTURE_2D` at
`linux/mod.rs:23` is dead there, so the Linux path transfers no better than the
Android one. A second build environment is required because the flake cannot
produce one.

Avoidable: the same five items as iOS.

Worth noting separately, because it is a product question rather than a porting
one: the gesture layer assumes a touch screen. Three-finger tap is the only
route to the on-screen keyboard and the controls (`touchpad.dart:95`), and
`session_screen.dart:280` calls `HapticFeedback`. Mouse input is already
handled, but `Listener` at `touchpad.dart:368-375` receives only in-window
motion, and a desktop remote desktop client needs pointer capture, which does
not exist.

### What moving code now would save

Three moves, in the order they get more expensive.

**Rename `crates/tachyx-video/src/android/bitstream.rs` out of the `android`
module.** It is portable H.264 arithmetic that every future decoder needs, it is
already compiled on every target, and `lib.rs:46-48` already admits the module
name is a lie about its contents. This is a rename today.

**Lift the policy half of `android/present.rs` out of the platform module.**
Queue depth, resync gating, stall reasons, latency accounting and the
surface-generation comparison are decisions about a decode loop, not about
MediaCodec. Only `android/decoder.rs` is genuinely platform. This is an
afternoon today and a week once a second platform has its own copy.

**Replace the `tachyx/video` channel and the overloaded `engine_handle: i64`
with a typed handle.** One integer currently means two different things and is
documented as one. A third platform makes that a bug rather than a wart, and the
typed FFI to carry it already exists in the same application.

None of the three is required to ship the current milestone. All three are
cheaper before the second client than after it, and the second client is what
they are for.

---

## Where this is guessing

Everything in this document was read. Nothing was run, no second machine was
tried, and no phone other than the one was attached.

The arithmetic on the transport send buffer is mine and is **inferred**. I
computed `video_send_buffer(1920x1088)` at 303,778 bytes and the gate's
requirement for a 250 kB key frame at 334 kB by hand from the formulas at
`video.rs:100-113` and `:378-390`. I did not run it. The measured key frame at
3440x1440 is 192 kB and does fit, so the failure is content-dependent and I
cannot say how often a real session crosses the line.

The behaviour of GNOME and KDE is **inferred** from what those compositors are
known to implement upstream, not from running the daemon on either. The claim
that the idle inhibitor's roleless-surface trick is Hyprland-specific is
reasoning from the protocol text and from the code's own comment, not a
measurement on a second compositor.

That an unknown GStreamer property name panics rather than erroring is **read**
from the glib API contract, not exercised here. That the nvcodec elements do not
import DMA-BUF is **inferred**.

The claim that `OPENING_QUALITY` is systematically wrong at other resolutions is
reasoning about how video encoders behave, not a measurement. The direction is
confident; the magnitude is not.

The iOS backspace behaviour is flagged as an inference in its own paragraph and
is the single item in this document most likely to be wrong.

Where a section says "measured", the measurement is somebody else's and is in
[`findings.md`](findings.md) or [`congestion.md`](congestion.md). This document
added no measurements of its own, and every recommendation in it should be
treated the way [`roadmap.md`](roadmap.md) says to treat anything that was not
run.
