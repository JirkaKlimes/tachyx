# What this build assumes

The build runs on one setup: a NixOS laptop with Hyprland, an AMD Radeon 780M,
two outputs at 2880x1800 and 3440x1440, and a Galaxy S25 on Wi-Fi. Nearly every
number in the code was measured on that pair. [`findings.md`](findings.md) and
[`congestion.md`](congestion.md) record which figures were run and which were
reasoned.

This document asks one question: what happens when the build meets a machine
that is not this one. That means a different compositor, GPU, phone or
platform, or a slow link. Items are ordered by the cost of the fix, not by
subsystem, so the document is a work queue.

Three words have fixed meanings, as in the other spec documents. **Read**: the
claim comes from the source, and nothing was run. **Measured**: a figure in the
repository that was taken on the target machine. **Inferred**: a conclusion from
reading plus arithmetic, with no run behind it. The last section collects the
guesses.

Every location was read. Nothing was executed.

---

## Cheap fixes: what breaks on the next machine

Most of these are a few lines. Each one stands between a new machine and a
picture.

### Per-device permissions are recorded and never enforced

`Permissions` (`tachyx-crypto/src/pair.rs:45-57`) holds `control` and
`unattended`. `tachyx-hostd pair --view-only` promises "The device cannot type or
click", and `--attended` promises that somebody at the machine accepts each
session (`cli.rs:123-155`). `pairings set` changes both, and `pairings` shows
them (`show.rs:262-267`, `control.rs:260-261`).

No session code reads either field. `session.rs:784-790` hands every input
batch to injection. `daemon.rs:522-561` opens a session for every paired device,
with no accept step. So a device paired view-only controls the host, and an
attended device opens a session that nobody accepted. **Read**, not run.

This is not a machine assumption, but it is the most urgent item here, because
the CLI tells the operator the opposite. Fix for `control`: carry the paired
record into the session (the lookup already happens at `daemon.rs:537-546`) and
drop input from a view-only device. `unattended: false` needs an accept flow
that does not exist. Until it does, refuse the session and say why.

### Injection resolves an unset output to `eDP-1`

`config.rs:57` documents an unset `output` as "the first output the compositor
reports". Capture does this: `State::find_output` (`capture.rs:1293-1321`) takes
the first output for an empty name. Injection does not: `daemon.rs:250` passes
`unwrap_or("eDP-1")` to `Injection::spawn`. `eDP-1` is the internal panel of the
development laptop.

- A host with no `eDP-1` and no configuration file does not start. The error is
  `no output named eDP-1`, and it comes before the socket is bound.
- **Inferred:** on a laptop whose compositor reports another output before
  `eDP-1`, capture serves one screen and injection targets another.

Fix: resolve the output once, in one place, and pass the same name to capture
and to injection.

### A compositor that is not wlroots fails with the wrong error first

Capture needs `ext-image-copy-capture-v1`. Injection needs the wlroots virtual
pointer. GNOME has neither. KDE has the capture protocol on recent versions and
has never had the wlroots pointer. The tree has no `zwlr-screencopy-v1` fallback
and no portal path; the portal path was cut on purpose (see
[`findings.md`](findings.md), "Portal RemoteDesktop does not exist on this
compositor").

`tachyx-hostd doctor` checks all seven required globals at once and names each
missing one (`doctor.rs:24-38`, `:229-256`). The daemon does not run that check
at start. `run` (`daemon.rs:206-250`) probes the encoders and then starts
injection. `Injector::new` (`input/lib.rs:152-187`) checks the xdg output
manager, then the named output, then the virtual pointer. So on GNOME or KDE the
first error is `no output named eDP-1`. With `output` set, the next error is
`compositor does not offer zwlr_virtual_pointer_manager_v1`.

Fix: run the doctor's compositor check at start, before any name is resolved,
and refuse with its sentence.

A smaller case of the same problem: `capture.rs:524` and `:527` bind the two
ext protocols at the advertised version. Every other bind in the tree uses
`version.min(n)`. Both protocols are at version 1 upstream (read). A compositor
that advertises version 2 hands back a proxy whose events the generated code
cannot decode. **Inferred:** that surfaces as a protocol error and a
`DispatchError`, not as a clear message.

### The lost-host threshold is sixteen heartbeats, and four places say three

`client/session.rs:71` sets `LOST_AFTER` to
`HEARTBEAT_INTERVAL.saturating_mul(16)`, which is 4 s. The reason is at
`:62-70`: a heartbeat is a stream frame, quinn puts datagrams in a packet ahead
of stream frames, and a three-beat threshold fired on Wi-Fi while video still
arrived (measured).

Four places still describe three beats:

- `client/session.rs:120`: "three heartbeat intervals".
- `client/session.rs:170`: "lost three heartbeats after the connection opened".
- `client-ffi/api/session.rs:75`: the same sentence as `:120`.
- `proto/control.rs:480-482`: "Three missed beats are then noticed three
  quarters of a second in". This paragraph is the justification for
  `HEARTBEAT_INTERVAL` itself.

The test at `client/session.rs:1237-1246` checks `LOST_AFTER` against itself and
checks that two beats are `Idle`, so nothing catches the drift. It is a
documentation fix. It matters because `control.rs` is where the next person who
tunes the heartbeat starts.

### The cursor interval assumes 60 Hz, and the client's smoothing depends on it

`hostd/cursor.rs:37` sets `INTERVAL` to 16 ms, "one video frame at sixty hertz"
(`:32-36`). Neither output on the development machine runs at 60 Hz. The
client's `BLEND = 0.35` (`client/cursor.rs:43`) is justified at `:37-42` as
clearing an error "inside a tenth of a second" at sixty reports a second. So the
client's smoothing time constant depends on a host constant, and neither file
names the other.

Either rate is defensible. The hidden coupling is not. Derive the cursor
interval from the capture refresh, or state in both files that 16 ms is a wire
fact both ends share, the way `HEARTBEAT_INTERVAL` is stated.

### Idle inhibition is required, and its mechanism is one compositor's

`session.rs:115` runs `let _awake = crate::idle::Inhibitor::new()?;` before the
pipeline starts. A compositor without `zwp_idle_inhibit_manager_v1` cannot serve
a session, although the picture does not depend on it. The requirement is
deliberate (`doctor.rs:24-29` gives the reason: a blanked screen produces no
frames). A session that can blank is still better than no session.

The mechanism is also specific. `idle.rs:31-36` says the inhibitor's surface is
never committed and has no role, and that "this one resolves that by treating
it as visible". That is a statement about Hyprland, not about the protocol.
**Inferred:** on a compositor that treats such a surface as not visible, the
inhibitor is created, no error is raised, and the screen blanks mid-session.
The silent failure is worse than the loud one.

Fix: log a warning and continue when the protocol is missing. State in the
module that the visibility rule is Hyprland's.

### A second client is not refused, it waits until its handshake times out

`daemon.rs:522-561` awaits `session::serve` inside the accept loop. One session
at a time is therefore structural. That is acceptable for this milestone (see
[`acceptance.md`](acceptance.md)).

The problem is what the second client sees. `host.accept()` is not polled while a
session runs, so the second handshake never completes. The second client waits
until its own timeout and is told nothing. For the same reason, one client that
is slow to finish its handshake inside `Host::accept` (`endpoint.rs:359-365`)
holds the loop for everyone else.

The parts of the fix exist. The close code `REFUSED` is at `endpoint.rs:34-35`,
and the client turns a close reason into `TransportError::Refused`
(`endpoint.rs:552-568`). One `accept` in a `select!` that closes the newcomer
with a "busy" reason turns a timeout into a sentence.

### A client that disconnects mid-gesture leaves keys and buttons held

`Injector` tracks held modifiers in `held` and `locked` (`input/lib.rs:136-137`).
It tracks no pointer buttons. Nothing releases either. One `Injection` lives
for the life of the daemon and serves every session (`daemon.rs:244-250`).

So a client that disconnects while it drags, or with Shift down, leaves the
host with a button held or a modifier latched. Only the real key on the host
clears it. The client releases its held keys and buttons
(`keyboard.dart:136-150`, `touchpad.dart:337-345`) when the app leaves the
foreground (`session_screen.dart:239-247`). A lost connection cannot carry
those releases.

Fix: the injector records every key and button that is down, and the session
releases all of them when it ends. This is the difference between a dropped
connection and a machine somebody has to walk over to.

### The host's send buffer is sized for 1920x1088, and a client can ask for more

`daemon.rs:634-639` sizes the transport once, before any client connects, for
`CodedSize::new(1920, 1088)`. `video_send_buffer` (`video.rs:138-169`) gives
307,138 bytes for that size. The buffer never changes after that.

The ladder's top rung is bounded only by the client's viewport
(`session.rs:1121-1151`). A Linux client with a 2560x1440 window against the
3440x1440 output gets a top rung of 3440x1440.

The gate (`queue.rs:179-193`) charges each datagram its payload plus 16 bytes of
header plus 32 bytes of quinn accounting, and it counts parity. Parity opens at
20%, which is 30% on a key frame (`video.rs:298`, `packet.rs:240-266`). At a
1418-byte payload, a key frame above about 227 kB needs more than 307,138 bytes
and is refused. **Inferred**, by hand, not run.

Two measured key frames bracket that limit. One at 3440x1440 was 192 kB
([`congestion.md`](congestion.md), "Pacing"). Key frames at 2580x1080 and
8 Mbit/s averaged 287.2 kB ([`findings.md`](findings.md), "Nothing schedules a
key frame any more"). So the failure depends on content.

When a key frame is refused, the client sees a hole and asks for another key
frame, which is refused too. The watchdog (`session.rs:521-552`) notices that
nothing reaches the wire, drops the controller to the floor, and forces a key
frame. A key frame at the floor rate is smaller. Whether it is small enough at
3440x1440 is not known.

Fix: size the buffer from the coded size the session chose, which is known when
`stream_config` is sent. Or refuse a top rung the buffer cannot hold, and say so.

### The development shell assumes NixOS and zsh

- `flake.nix:144-145` put `/run/opengl-driver/lib` on `LD_LIBRARY_PATH` and set
  `LIBVA_DRIVERS_PATH` to `/run/opengl-driver/lib/dri`. Those paths exist only
  on NixOS. The README names `nix develop` as the way to get a toolchain. On
  Debian or Fedora with Nix, the shell builds and the VA-API driver is not
  found.
- `flake.nix:167-170` runs `exec zsh` for every interactive shell.
- `crates/tachyx-client-ffi/rust-toolchain.toml:7-9` names `channel = "nix"`.
  That toolchain exists only after `nix develop` links it (`flake.nix:149`), so
  `flutter build` has no Rust toolchain outside the dev shell. The file says so.

Fix: fall back to the system paths when `/run/opengl-driver` is absent, and keep
the shell that started the environment.

### Smaller items

`AddHostForm` (`host_list.dart:152-232`) is unused. It joins host and port as
`'$host:$port'`, which breaks an IPv6 literal. Its placeholder is
`192.168.1.20`, one network's address. `HostLink.defaultPort`
(`link.dart:86-91`) has no other user. Delete both or fix the join.

`LAN_MTU` is 1472 (`endpoint.rs:37-41`): "a 1500-byte path less twenty bytes of
IPv4 and eight of UDP". The client binds IPv6 first (`driver/lib.rs:553-561`)
and uses `LAN_MTU` for the direct path. Over IPv6 the header is forty bytes, so
the figure is 1452. **Inferred:** path discovery lowers it, so the cost is a few
dropped packets at the start of an IPv6 session, not a failure.

`configure_encoder` (`encode.rs:841-898`) writes properties with `set_property`
and `set_property_from_str`. Its doc says an unknown name panics, and it keeps
base-class and codec properties apart for that reason. A GStreamer release that
renames one of them still kills the encode thread instead of returning
`Error::MissingElement`. **Read from the glib API contract, not run.** A
`find_property` check per name turns the panic into an error.

`VideoStats` (`client-ffi/api/session.rs:96-112`) does not carry
`parity_received` or `frames_recovered`, although `ReassemblyStats` counts both
(`packet.rs:779-785`). So the stats overlay (`stats_overlay.dart:186-191`)
cannot show whether forward error correction works. It is the newest mechanism
in the system, and the one tool built to observe the system cannot see it.

---

## Expensive fixes: the shape is wrong

Each of these is a decision made in the wrong place. Each costs more with every
week of code built on top of it.

### A second encoder backend is a redesign, not a new element name

NVENC works on the test machine, and no backend for it exists
([`findings.md`](findings.md), "Encode"). VA-API is wired through more than the
element names:

- `ENCODERS` (`encode.rs:50-64`) lists only VA elements, and says why: the caps
  between the scaler and the encoder are `memory:VAMemory` (`encode.rs:740`), so
  an encoder that cannot take a VA surface has nothing to link to.
- The scaler is `vapostproc` (`encode.rs:649`).
- `configure_encoder` writes `GstVaBaseEnc` properties. `target-percentage`
  (`:862`) and `mbbrc` (`:870`) have no NVENC counterpart. `cpb-size`
  (`:760-761`) is `vbv-buffer-size` on NVENC. `rate-control` (`:853`) and
  `key-int-max` (`:868`) are spelled differently. A wrong name panics.
- `EncodeSize::MIN` and `MAX` (`media/lib.rs:254-261`) are the VA pad template
  limits.

The deepest coupling crosses a file boundary. `import_layouts`
(`encode.rs:899-935`) reads the DRM formats from the DMA-BUF sink templates of
`vapostproc`. `choose_layout` (`capture.rs:1356-1384`) demands a format and
modifier that are in both the compositor's list and that element's. So **the
capture allocation format depends on which encoder plugin is installed**, and
with no VA plugin the failure comes from inside capture. That is a redesign of
the negotiation, not a swap.

The buffer handed to GStreamer is a DMA-BUF with a `GstVideoMeta`
(`encode.rs:776-810`). **Inferred:** the nvcodec elements do not import DMA-BUF,
so an NVENC path needs an upload. That is the PCI Express copy that
[`findings.md`](findings.md) ("Encode") names as the cost of NVENC.

What this needs: a backend that owns four things. The element names, the memory
feature string, the property map, and the importable-layout query. Then capture
stops depending on a VA element. Intel is nearly free once that exists, because
it uses the same elements (`vah264lpenc` and `vaav1lpenc` are already in
`ENCODERS`). NVIDIA is real work either way, and less of it behind a seam.

### Capture and injection have no seam

A second compositor is a larger version of the same problem. Nothing is behind
a trait yet.

Injection speaks Wayland requests directly: `Injector::apply` and everything
under it (`input/lib.rs:223-395`). `keymap.rs` exists only to build an XKB
keymap, so that an on-screen keyboard can type a character no host key makes.
The portal RemoteDesktop interface takes keysyms, not keymaps, so that file has
no counterpart there. It is a different transport, not a different name.

Capture is the same. `Pump::setup` (`capture.rs:925`) and the dispatch impls
(`capture.rs:500-714`) are the protocol. A portal and PipeWire path replaces all
of it. `PointerCapture` (`capture.rs:787-797`, `:1474-1597`) reads the
compositor's cursor session, and the portal path loses that separate cursor
channel ([`findings.md`](findings.md), "The portal capture path is not a latency
improvement"). The client-drawn pointer, which the input design depends on,
would go with it.

None of this needs building now. What it needs: `hostd` depends on a capture
trait and an injection trait, not on `tachyx_media::Capture` and
`tachyx_input::Injection` directly. Then a second implementation is an addition,
not a rewrite of the session loop. That is cheap now, and dearer with every new
call into either.

### One measured key frame sizes the buffers

`MEASURED_KEY_FRAME_BYTES = 130_427` (`video.rs:53`) is one 1080p key frame from
one encoder on one machine. `video_send_buffer` (`video.rs:158-169`) scales it
by pixel count against `MEASURED_KEY_FRAME_PIXELS` (`:59`). It also opens the
sender's key frame estimate (`:308`) and the pacer's rate (`:311`).
`Budget::new` (`idr.rs:83`) opens on a flat `130_000`. Tests repeat it at
`pace.rs:213` and `encode.rs:1213`.

A key frame's size follows the picture's detail and the quantiser, not its area.
Measured: 287.2 kB at 2580x1080 and 8 Mbit/s, which is 2.2 times the 1080p
figure for 1.34 times the pixels. Pixel scaling is a defensible shape for a
buffer bound. It is not an estimate.

After the first key frame the code follows measurement. The budget reads the
encoder's measured key frame size before every request (`session.rs:390-405`).
The pacer paces each picture at its own weight on the wire (`video.rs:525-540`).
What remains is the buffer, which is sized before a client exists
(`daemon.rs:634-639`). See the send buffer item above.

### The client cannot say what it can decode

`Capabilities` (`proto/control.rs:194-221`, documented in
[`protocol.md`](protocol.md)) carries a viewport, a maximum decode size, two
frame rates, a bitrate ceiling, and a codec set. The codec set is probed on the
client machine. Nothing in it states a profile, a level, a bit depth or a chroma
format. [`architecture.md`](architecture.md), section 5, lists HEVC, 4:4:4,
10-bit and HDR as not built, and each one needs such a field.

`max_decode` is not a decoder capability. `capabilities()`
(`client/session.rs:95-105`) sets it to the viewport, with a good argument at
`:81-94`: a picture larger than the surface is waste. The host uses the field as
a limit (`session.rs:1148`). The client never asks the platform decoder for its
maximum size, profile or level.

That works because the one phone's decoder exceeds its own panel. It stops
working on the first device where the two differ. It has no answer for a client
without High profile, or for H.265. Adding the fields is cheap now, at protocol
version 4 (`proto/lib.rs:57`) with two clients. It is a migration once there are
more.

The host announces almost nothing in return. `HostHello`
(`proto/control.rs:307-337`) carries the version and whether audio works. It
has no encoder list and no monitor list, which section 5 of the architecture
designs. A client cannot choose a monitor,
because it cannot learn that there is more than one. `client/cursor.rs:227-236`
treats "the pointer is on another monitor" as "draw nothing". That is the only
multi-monitor behaviour in the system.

### Portable decode code lives in the Android module

**The Kotlin is fine.** `VideoSurface.kt` is 130 lines, and nearly all of it is
mechanism only the JVM can perform: `TextureRegistry`, `createSurfaceProducer`,
the `SurfaceProducer.Callback` interface, `System.loadLibrary`. It holds no
codec string, no colour format, and no bitstream handling.
`handlesCropAndRotation()` is reported to Dart (`VideoSurface.kt:82`), not acted
on. `MainActivity.kt` adds the Wi-Fi lock and picture-in-picture, which are
platform work too.

Three small things in Kotlin are worth moving:

- `VideoSurface.kt:51` defaults a missing width or height to `1`. The result is
  a one-pixel image reader and a black screen with no error. Dart already holds
  the typed value. `present.dart:94` has the same habit and defaults a missing
  id to `-1`.
- `VideoSurface.kt:94` repeats the idempotence check that
  `session_screen.dart:354-362` already makes.
- The `open`/`resize`/`close` verbs with string keys `id`, `width`, `height` and
  `appliesCrop` are a hand-built RPC beside a typed FFI that exists in the same
  application.

**The real cost is in Rust.** `crates/tachyx-video/src/android/bitstream/` is 872
lines of pure arithmetic: an Annex B walker and SPS parser for H.264 (`h264.rs`)
and an OBU and sequence header reader for AV1 (`av1.rs`). Nothing in it calls a
platform API. `lib.rs:48-51` and `android/mod.rs:15-17` say so, and compile it
everywhere. It still sits inside a module named `android`. Every other decoder
needs the same code: VideoToolbox builds its format description from the
parameter sets.

`android/present.rs` is 553 lines, and only `android/decoder.rs` under it is
really MediaCodec. The rest is policy any decode loop wants: queue depth and
drop-to-resync (`present.rs:16-23`, `:197-210`, `:352-358`), the stall reasons
(`:363-365`, `:452-490`), the latency accounting (`:368-378`), and the
surface-generation check (`:504-530`).

Moving `bitstream/` out of `android/` is a rename now. Splitting the policy
half out of `present.rs` is an afternoon. Both get harder as the module grows.
Both decide whether an iOS client writes a decoder or a whole decode subsystem.

The `surface` module of `tachyx-unsafe` has the right shape and the wrong types.
`Producer`, `Surface` and the generation counter are the abstraction a second
platform wants. They are concrete types behind `#[cfg(target_os = "android")]`
(`tachyx-unsafe/src/lib.rs:24-25`), with no trait to implement.

### The opening bits-per-pixel figure is one measurement at one resolution

`OPENING_QUALITY = 0.06` (`ladder.rs:23-32`) comes from a busy quarter-screen
window at 1080p60 that carried 7.86 Mbit/s: 0.063 bits per pixel per frame.
`worst_case` (`ladder.rs:396-400`) applies it at every rung, times that rung's
pixel count.

Bits per pixel are not constant across resolutions for desktop content. The
same window at twice the linear size carries the same detail over four times the
pixels, so the cost per pixel falls. **Inferred:** the figure over-estimates on
large captures, so a session on a 4K host opens lower than it needs to, and it
under-estimates on small ones. It was also measured on H.264 and is applied to
AV1, which needs fewer bits for the same picture. **Inferred:** AV1 sessions
open lower than they need to. The effect is bounded: the figure chooses only the
opening point, and the ladder follows real production after the first second.
But it decides the first picture anyone sees.

Two more constants are stated against one monitor:

- `MAX_STRIDE = 5` (`ladder.rs:34-38`) is "15 frames a second on a 75 Hz
  output". It is a stride, so the floor is 24 frames a second at 120 Hz, where
  the ladder has no rung left for a bad link, and 6 at 30 Hz, far below the
  "stops feeling connected" line it exists to hold. The refresh it divides is
  the output's, capped by the client's requested rate (`session.rs:160-166`).
- `MIN_TARGET = 300_000` (`congestion/lib.rs:43-49`) exists because "the
  smallest rung of the ladder, 860x360" needs 279 kbit/s. 860x360 is 3440x1440
  divided by four.

Derive all three: the stride floor from a target frame rate, the rate floor from
the smallest rung this capture has.

### Nothing reads a display transform or a colour space

Nothing in the tree reads the `wl_output` transform or the transform event of an
ext capture frame. **Inferred:** a rotated monitor is captured in its buffer
orientation and injected against its logical extents, and nothing reconciles
the two.

Colour is absent. `choose_layout` (`capture.rs:1356-1384`) accepts four
formats, all 8-bit RGB. A compositor that offers only 10-bit formats fails with
`Error::NoSharedFormat`, which is loud but says nothing useful. `StreamConfig`
(`proto/control.rs:398-415`) carries codec, sizes, aspect, frame rate and
bitrate. It has no transfer function, no primaries and no range. **Inferred:**
an HDR or wide-gamut desktop that negotiates is sent unlabelled, and the client
draws it as sRGB.

None of this needs solving now. `CaptureFormat` and `StreamConfig` need a place
for the answer, so that adding it later is a field and not a protocol version.

### The frame deadline and the reorder window cross at the ladder's floor

`client/session.rs:28-34` states the invariant: the 50 ms `FRAME_DEADLINE` is a
fallback for the frame nothing follows, and "every other case is decided
sooner, by the reassembler's own reorder window". The window is one frame
period (`:284-291`), which is the right derivation.

One frame period is under 50 ms only above 20 frames a second. The ladder's
floor is 15 frames a second at 75 Hz, which is a 66.7 ms window. At the slowest
rung the fallback fires first and the invariant is inverted, on exactly the link
that drove the ladder down.

Fix: derive the deadline from the same frame rate, or take the larger of the
two.

A second copy of the same fact crosses a crate boundary. `FRAME_SPAN = 35 ms`
(`pace.rs:55-64`) is justified because "the receiver gives a frame 50 ms from
its first datagram". That 50 is `client/session.rs:34`, and `tachyx-transport`
cannot depend on `tachyx-client`. The number is retyped in a comment. It belongs
in `tachyx-proto` next to `HEARTBEAT_INTERVAL`, which solved the same problem by
putting one wire fact in one place and saying why.

### Facts written twice

The port shows the right pattern: `config.rs:13-17` and `link.dart:86-91` point
at each other. The rest need two edits each. Ordered by the cost of a wrong
edit:

**The coded size is derived twice from separate sources, and nothing compares
them.** The host computes it in `stream_config` (`session.rs:1192-1220`) from
`EncodeSize::coded` (`media/lib.rs:323-327`). Dart passes it to
`VideoSurface.kt` to size the image reader (`present.dart:113-120`). The decoder
parses its own copy from the bitstream (`android/bitstream/h264.rs:53-108`,
`av1.rs`) and gives that to MediaCodec (`android/decoder.rs:79-80`). If the two
disagree, the compositor reads a buffer of the wrong shape, which is the hazard
`session_screen.dart:349-353` warns about. **Inferred:** for AV1 they may
already disagree. `EncodeSize::coded` rounds to sixteen for every codec, while an
AV1 sequence header states the frame size itself; the VA test vector at
`av1.rs:244-247` declares 1920x1082.

**The JNI symbol names encode the Kotlin package and class.** They are defined at
`surface.rs:117`, `:143` and `:163` in `tachyx-unsafe`, and matched by the
package at `VideoSurface.kt:1` and the `VideoSurfaceNative` object at
`:117-130`. `surface.rs:15-16` says not to rename them. The Kotlin side has no
such note. Moving the Kotlin package, which usually happens with a new
`applicationId` (`app/build.gradle.kts:8`, `:19`), breaks the link at run time
with no build error.

**Annex B parsing exists twice in Rust**: the host's filler stripping
(`encode.rs:1083-1120`) and the client's walker
(`android/bitstream/h264.rs:110-150`).
Both write out the NAL type mask `& 0x1f` (`encode.rs:1090`, `h264.rs:32`). The
Linux client uses `h264parse` (`linux/decoder.rs:42`).

**The HID keyboard page** is `HidUsage::KEYBOARD_PAGE` (`proto/input.rs:50-53`)
and `keyboardPage` (`keyboard.dart:9-14`), because the frb mirror
(`client-ffi/api/input.rs:37-46`) does not carry the constant across.

**The method channel contract** is `'tachyx/video'`, four argument keys and three
method names. It is written at `present.dart:13` and `:86-125`, and at
`VideoSurface.kt:26` and `:35-51`. Neither side says "must match".

**Smaller copies:**

- The 5 s transport idle timeout is set at `endpoint.rs:99` and restated in
  prose at `proto/control.rs:481` and `driver/lib.rs:86`.
- The 1.53 s capture idle gap appears at `client/session.rs:58`, `:110` and
  `:1096-1119`, `client-ffi/api/session.rs:66`, `proto/control.rs:420` and
  `proto/packet.rs:973`.
- The macroblock alignment of 16 is `CodedSize::ALIGNMENT`
  (`proto/control.rs:56`) and a literal in `EncodeSize::coded`
  (`media/lib.rs:324`).

One more that is not duplication. `proto/control.rs:43-49` says every ladder
rung is a multiple of sixteen, "which avoids the problem rather than detecting
it". The code does not do that. `EncodeSize::fit` (`media/lib.rs:286-306`)
rounds the visible size to even, and its inline comment says so, against its own
doc line. `ladder.rs:77` hedges with "wherever that is free". `coded()` then
rounds up to sixteen, and the difference is a real crop. The client corrects it
with `VideoCrop` (`present.dart:127-175`, `video_texture_view.dart:66-77`). The
test vector at `android/bitstream/h264.rs:252-254` records a live 2580x1080
visible against 2592x1088 coded. So `CodedSize` never rejects anything, the crop
is real, and the note on the type describes a different system. A reader who
trusts the note may delete the crop handling.

---

## Defaults that are fine, and should say so

A number can be a good default and still need a line that says it is one, so
that the next person knows they may change it.

[`congestion.md`](congestion.md) does this in its "Confidence" section. It names
the 2.0 Mbit/s start, the 150 kbit/s control reserve, the 100 ms bucket ceiling,
the 0.8 and 1.3 retarget band, the 10% and 60% ladder thresholds and the 15
frames a second floor as guesses. It says every constant in the estimator and
the probe controller is libwebrtc's, not tuned here. Nothing below needs more
than the same treatment.

The GCC constants in `congestion/delay.rs`, `rate.rs`, `loss.rs` and `probe.rs`
are libwebrtc's published values. They work on paths far worse than this one,
and none has been tuned here. `THRESHOLD_MIN = 6.0` (`delay.rs:70`) is the one
`congestion.md` flags as most likely wrong on Wi-Fi.

`PACE_PERCENT = 250` (`pace.rs:51`) is libwebrtc's `kDefaultPaceMultiplier`. Its
doc explains why it cannot be lower (the encoder's measured overshoot) and why it
is not higher. `MAX_BURST = 25_000` (`:74`) and `BURST_SPAN = 15 ms` (`:88`)
were measured here, with the mechanism written down. `FRAME_SPAN = 35 ms`
(`:64`) is the one to revisit, for the reason in the section above.

`Parity::FLOOR = 5`, `CEILING = 50` and `MEASURED = 20` (`packet.rs:183-193`)
are a sensible clamp around one Monte Carlo. The key frame boost of half again
(`packet.rs:240-266`) is the lower half of libwebrtc's, and says why. The
percentage-of-datagrams shape generalises. The level is a starting point, and
the receiver report now drives it (`session.rs:257-262`).

`REORDER_WINDOW = 15 ms` (`packet.rs:763`) is documented as a floor under a
value derived from the frame rate. Every other measured constant wants that
treatment.

These are reasonable and none is a law: `Budget::SHARE = 0.15` and
`MAX_INTERVAL = 4 s` (`idr.rs:71`, `:74`), `RING = 3` (`capture.rs:56`),
`QUEUE_DEPTH = 8` (`android/present.rs:23`), `BUCKET_CEILING = 100 ms`
(`encode.rs:74`), `INPUT_TICK = 4 ms` (`driver/run.rs:26`), `SNAP = 0.05` and
`BLEND = 0.35` (`client/cursor.rs:35`, `:43`), `holdDelay = 500 ms`
(`touchpad.dart:169`). Several carry a 60 Hz justification for a value that is
fine at other rates. Saying so is cheaper than deriving them.

Port 47989 is arbitrary and unregistered. It is cross-referenced. It needs one
line that says it was chosen, not assigned. The 20 Mbit/s client ceiling
(`client/session.rs:79`) and the 40 Mbit/s host ceiling (`config.rs:96`) are
ceilings above a controller that works below them, which is the right
relationship.

`WHEEL_DETENT = 15.0` (`input/lib.rs:483`) is the Wayland convention for one
notch, not a measurement.

---

## What a second client costs

### iOS

There is no `ios/` directory. `app/` holds `android/` and `linux/` only, and
`flake.nix:17-21` targets two Linux systems, so the repository has no build host
for it either.

`present.dart:30-36` dispatches on `Platform.isAndroid` alone, so iOS falls into
`_Registered`, which ends in `tachyx_video::unsupported`. That module's
`decoders()` returns `Codecs::NONE` (`unsupported.rs:26-28`). So an iOS build
compiles, links, runs and connects, advertises no decoder, and the host ends the
session with `NoSharedCodec` (`session.rs:1159-1168`). That is the starting
position.

The unavoidable work, which no code move removes:

- The Xcode target, `Info.plist`, signing, and an `AppDelegate` that does what
  `MainActivity.kt:97-101` does.
- `NSLocalNetworkUsageDescription`. Without it, iOS 14 and later blocks a
  connection to a private address. Android has no counterpart.
- `VTDecompressionSessionCreate` and
  `CMVideoFormatDescriptionCreateFromH264ParameterSets`.
- The `FlutterTexture` protocol and `FlutterTextureRegistry`. These match
  neither the Linux `FlTextureGL` path nor the Android surface producer.
- Annex B to AVCC conversion. Nothing in the repository does this, because the
  host emits byte-stream format and both current clients take it as is.
- `kVTDecompressionPropertyKey_RealTime` and its neighbours, a different
  vocabulary from `android/hints.rs`.
- A frame lifetime rule across the pull callback. Linux solved this with
  `HELD_FRAMES = 3` (`linux/mod.rs:35`), and Android avoids it.
- A sound output. `tachyx-audio` has one for Linux and one for Android
  (`tachyx-audio/src/lib.rs:47-58`). The jitter buffer, Opus decoder and volume
  are portable, so only the device write is new.

Under all of that is one architectural difference. The Android design is "the
decoder writes into the compositor's buffer and no frame passes through code
this project owns" (`android/mod.rs:3-8`). iOS has no equivalent: VideoToolbox
hands back a `CVPixelBuffer` through a callback. So the central claim of the
client decode path does not transfer, and the iOS path will look more like the
Linux one than the Android one.

The work that exists only because of where code sits now:

- The `tachyx/video` method channel written a third time in Swift, with its
  silent defaults.
- A third meaning for `engine_handle: i64`. It already means an irondash engine
  handle on Linux and a surface producer id on Android, and
  `client-ffi/api/session.rs:432-442` documents only the first. That is an
  integer where a sum type belongs, and a third platform makes it untenable.
- The H.264 and AV1 parsers reached through `mod android`, or copied.
- The policy half of `present.rs` rewritten around a callback-driven decoder.

Everything below the presentation layer is free. `tachyx-client`,
`tachyx-driver`, `tachyx-transport`, `tachyx-relay` and `tachyx-crypto` depend on
quinn, rustls with ring, tokio, postcard and serde, and on nothing
platform-specific. `tachyx-sched` is a no-op off Linux
(`tachyx-sched/src/lib.rs:172-180`).
The ring choice (`transport/identity.rs:35-45`) was made for Android and helps
here too. The Dart input layer already handles `PointerDeviceKind.mouse` beside
touch, and `VideoCrop` is pure Dart.

Three Android-shaped things in Dart need attention, not reimplementation:

- `keyboard.dart:44-49` returns true from the key handler, justified by the
  Android back gesture. There is no `PopScope` anywhere, so the mechanism is
  Android-only and incomplete even there.
- `keyboard.dart:266` sets `enableIMEPersonalizedLearning: false`. It is
  Android-only, and it keeps a typed password out of the keyboard's learning
  store. On iOS that protection does not exist, and nothing says so.
- **Inferred, not read:** `keyboard.dart:207-216` detects backspace as a deletion
  against an empty document, which UIKit does not generally report. Backspace
  may never reach the host on iOS. That one needs a test before anybody claims
  support.

### Windows

There is no `windows/` directory. A Windows build compiles and links, advertises
no decoder (`unsupported.rs:26-28`), and the host ends the session.

Unavoidable: the runner scaffolding, a Media Foundation or D3D11VA decoder, a
`FlutterDesktopGpuSurfaceDescriptor` texture over a shared `ID3D11Texture2D`,
and a sound output. The `glctx` module of `tachyx-unsafe` is Linux-only
(`tachyx-unsafe/src/lib.rs:22-23`), and `TEXTURE_2D` (`linux/mod.rs:25`) means
nothing there, so the Linux path transfers no better than the Android one. The
flake cannot produce a Windows build environment.

Avoidable: the same items as iOS.

A product question, separate from porting: the gesture layer assumes a touch
screen. A three-finger tap is the only route to the on-screen keyboard
(`touchpad.dart:178-179`). `session_screen.dart:476` calls `HapticFeedback`,
which is harmless off a phone. Mouse input works, but the `Listener`
(`touchpad.dart:466-475`) receives only motion inside the window. A desktop
remote desktop client needs pointer capture, which does not exist.

### What moving code now saves

Three moves, in the order they get more expensive:

1. **Move `crates/tachyx-video/src/android/bitstream/` out of the `android`
   module.** It is portable H.264 and AV1 arithmetic that every future decoder
   needs. It already compiles on every target, and `lib.rs:48-51` already says
   the module name is wrong about it. This is a rename now.
2. **Lift the policy half of `android/present.rs` out of the platform module.**
   Queue depth, resync gating, stall reasons, latency accounting and the
   surface-generation check are decisions about a decode loop, not about
   MediaCodec. Only `android/decoder.rs` is platform code. This is an afternoon
   now and a week once a second platform has its own copy.
3. **Replace the `tachyx/video` channel and the overloaded `engine_handle: i64`
   with a typed handle.** One integer means two things and is documented as one.
   A third platform turns that into a bug. The typed FFI to carry it already
   exists in the same application.

None of the three is needed to ship the current milestone. All three are cheaper
before the second client than after it.

---

## Where this is guessing

No second machine was tried, and no phone other than the one was attached.

The send buffer arithmetic is **inferred**. `video_send_buffer(1920x1088)` comes
to 307,138 bytes from `video.rs:158-169`. The limit of about 227 kB for a key
frame comes from the gate at `queue.rs:179-193`, at a 1418-byte payload and the
opening parity level. Neither was run. Measured key frames fall on both sides of
the limit, so how often a real session crosses it is not known.

The behaviour of GNOME and KDE is **inferred** from what those compositors
implement upstream, not from running the daemon on either. The claim that the
idle inhibitor's roleless surface works only because of Hyprland is reasoning
from the protocol text and the code's own comment, not a run on a second
compositor.

That an unknown GStreamer property name panics is **read** from the glib API
contract and from the code's own comment, not exercised. That the nvcodec
elements do not import DMA-BUF is **inferred**.

That `OPENING_QUALITY` is wrong at other resolutions and for AV1 is reasoning
about how encoders behave, not a measurement. The direction is confident. The
size is not.

That the AV1 coded size disagrees with the bitstream is **inferred** from one
test vector and the rounding in `EncodeSize::coded`. No AV1 session on Android
was checked for it.

The iOS backspace behaviour is flagged as an inference in its own paragraph. It
is the item here most likely to be wrong.

Where a section says "measured", the measurement is in
[`findings.md`](findings.md) or [`congestion.md`](congestion.md). This document
adds no measurements. Treat every recommendation in it as unproven until it is
run.

---

## Resolved

Settled in the code, with the evidence:

- Unset output in capture: `State::find_output` takes the first output
  (`capture.rs:1293-1321`). Injection is still open, above.
- Key frame interval counted in pictures: nothing schedules a key frame;
  `key-int-max` is 1024 (`encode.rs:76-90`).
- Dart address parsers that broke on IPv6: gone. Hosts are dialled by device
  identifier (`link.dart:102-104`).
- Third uncommented port copy in `driver/examples/connect.rs`: gone.
- Three Rust target lists: the two `rust-toolchain.toml` files agree, and the
  flake's `abiVersions` selects emulator images only (read in nixpkgs
  `androidenv`).
- Two-finger scroll direction: the client negates finger travel
  (`touchpad.dart:415-428`, `proto/input.rs:57-66`).
- Consumer page keys: a stated choice now; volume keys step the host's volume
  (`keyboard.dart:16-36`, `:123-133`).
- A second codec: AV1 is negotiated per session (`proto/control.rs:107-125`,
  `session.rs:1159-1168`).
- Fractional scale: capture and injection both work in xdg-output logical pixels
  (`capture.rs:73-85`, `:477-482`).
- Parity that nothing drove: the receiver report sets it (`session.rs:257-262`).
- The Dart Annex B parser and the second Rust list of JNI names: gone.
