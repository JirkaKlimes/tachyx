# Roadmap

What is built, what comes next, and why. Each status comes from the code.
[`acceptance.md`](acceptance.md) defines when the current milestone is done.
[`assumptions.md`](assumptions.md) lists what is expected to break on a machine
that is not the test machine.

The project exists for latency and for behaviour on a link that is not perfect.
Feature count against Sunshine and Moonlight is not the goal.

## Built

- **Capture.** A wlroots output (developed on Hyprland) through
  `ext-image-copy-capture-v1` into a DMA-BUF that the encoder imports.
- **Encode.** VA-API through GStreamer. H.264 or AV1, negotiated per session
  from what each end probes. A resolution ladder spends bitrate first, then frame
  rate, then resolution.
- **Rate control.** A delay-based congestion controller reads a receiver report
  from the client ([`congestion.md`](congestion.md)). Reed-Solomon parity follows
  measured loss and stops on a clean link. The pacer releases each picture at a
  rate set by its own weight on the wire. The session socket marks its packets
  DSCP EF.
- **Cursor.** Sent apart from the picture and drawn on the client, which moves
  it on local input at once and corrects it from the host's reports.
- **Audio.** The host's sound (`pulsesrc`, Opus) with remote volume control.
- **Host behaviour.** Idle inhibition during a session. Realtime priority for
  the picture threads through RealtimeKit.
- **Identity and pairing.** One Ed25519 key per device. CPace pairing by typed
  code or QR. Mutual TLS that pins the other end's exact key. A view-only device
  gets no input. A device paired as attended is refused until the accept step
  below exists.
- **Relay** (`tachyx-relayd`). Presence, rendezvous, reflexive address, hole
  punch, and a splice fallback when the punch fails.
- **Host CLI.** `tachyx-hostd id | status | pair | pairings | sessions | relay |
  doctor`.
- **Clients.** Flutter on Linux and Android, with hardware decode and no CPU copy
  of a frame.

## Not built

- Coordination server, accounts and billing. Design only, in
  [`architecture.md`](architecture.md).
- iOS, macOS, Windows and web clients. Hosts on compositors that are not
  wlroots.
- Clipboard. More than one client at a time.
- A sandboxed process for capture and encode.
- Move of a relayed session to a direct path. `Routes::point`
  (`relay/socket.rs:153-160`) can re-point a peer's address, but only a test
  calls it, and the congestion controller does not yet handle a change of path.
- Direct libva encode.
- A local accept step for a device paired as attended.

## Where this differs from the prior art

Capture goes from `ext-image-copy-capture-v1` straight into a shared buffer the
encoder reads. On Hyprland the portal route is itself a screencopy client that
republishes frames through PipeWire, so it is the same path with more hops
([`findings.md`](findings.md), "The portal capture path is not a latency
improvement"). The frame's presentation timestamp is 0.04 ms old at the median
when capture reads it.

The rate is measured, not assumed. Parsec has published that it does not control
the rate. On a stable local network that is defensible. On a phone's hotspot it
is not, and that is the case this project is for.

Parity follows measured loss. Sunshine and Moonlight carry a flat fifth of
their bandwidth as parity ([`findings.md`](findings.md)). On a clean link
that fifth is waste. On a bad link it is not enough.

Recovery is where this build is behind. The GStreamer element exposes neither
intra refresh nor long-term reference frames ([`findings.md`](findings.md),
"Intra-refresh and long-term reference frames are unavailable"). So every lost
frame costs a full key frame. Measured: 130.4 kB against 16.7 kB for a delta
frame at 1080p60, and 287.2 kB against 30.2 kB at 2580x1080. The element also
floors the coded picture buffer at half the peak bitrate, which is 500 ms at
8 Mbit/s. Sunshine sizes its buffer to one frame ([`findings.md`](findings.md),
"The coded picture buffer cannot be made smaller than half a second").

Both limits belong to the element, not to the hardware. VA-API exposes intra
refresh, and `cros-libva` reaches it. The same route reaches the buffer through
`VAEncMiscParameterTypeHRD`.

## Next, in order

1. **Survive a link the stream overdrives.** On phone Wi-Fi the return path goes
   silent under load and does not come back, and the delay-based controller is
   blind at that moment, because its reports travel the path that died
   ([`findings.md`](findings.md), "The video path stalls, and the cause is a link
   this system overdrives"). Built so far: the watchdog drops the controller to
   its floor and forces a key frame (`session.rs:524-555`). Still needed:
   - a lower start than the 2 Mbit/s `START_TARGET` (`congestion/lib.rs:102`);
   - the packet rate as a limit of its own, since packets are already at the
     path MTU and cannot grow;
   - a reset of a dead path that drops the stale backlog, so that a stall costs
     a hiccup and not the session.

   Then prove the controller on a real link, not only a shaped one.
2. **Direct libva encode.** The largest latency win available. Intra refresh
   replaces the full key frame of each recovery (130 kB at 1080p) with intra
   columns spread over the frames that follow. Long-term references let the
   client recover from the last frame it acknowledged. A coded picture buffer
   of one frame removes the 500 ms the element now forces before rate control
   reacts.
3. **Pointer motion scaling.** The protocol says the host scales `PointerMotion`
   by the ratio of capture size to encode size (`proto/input.rs:69-74`). The host
   does not: `input/lib.rs:249-251` injects the deltas unchanged. So the pointer
   speed changes when the ladder steps. The client measures the gain
   (`client/cursor.rs:51-72`) to draw its own pointer in the right place, but
   the speed under the finger still changes.
4. **Relayed to direct.** Re-point a relayed session once a punch succeeds, and
   teach the controller that the path changed.
5. **Clipboard.**

After that, the remaining fast-follows of [`architecture.md`](architecture.md),
section 16, in their order.
[`assumptions.md`](assumptions.md) lists the code moves that make a second
client cheaper if they happen first.

## Method

Most of what is known was found by running the system, not by reading it.
Examples: a frame deadline that abandoned every key frame a third of the way
through its arrival; a cursor session whose completion event arrives only once
the mouse moves; a bandwidth estimate of zero on a healthy 100 Mbit/s path; a
key frame interval counted in frames on a capture path that produces no frames
when nothing moves. Reasoning about the code did not reach any of them, and
several contradicted the documentation of the component in question.

[`findings.md`](findings.md) holds what measurement established, including where
a later measurement overturned an earlier one. Keep writing to it. Distrust
anything in it that was not run.
