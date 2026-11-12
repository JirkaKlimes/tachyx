# What done means

Each crate proves its own part, and each proof can pass while the product is
unusable. Capture can reach 120 frames a second, the encoder can write a file
that plays, the transport can move frames over loopback, injection can reach the
compositor, and the phone can still show a smeared picture where taps land in
the wrong place.

So the milestone is not the sum of those proofs. It is the run below, measured.
[`roadmap.md`](roadmap.md) says what is built and what comes next.

## The run

A phone and the test laptop. The daemon is started with no arguments and no
window.

1. The phone pairs once, with the typed code or the QR code that
   `tachyx-hostd pair` shows. After that, the host is chosen from the phone's
   list, and no address is typed.
2. On the same network, the desktop appears.
3. From a mobile network, with a relay configured, the desktop appears, through
   the hole punch or through the splice.
4. A finger moves the pointer, and it feels like a trackpad, not like a delay.
5. A one-finger tap clicks the thing under the pointer. A two-finger tap opens
   the context menu. A three-finger tap shows the keyboard, and a second one
   hides it.
6. A two-finger drag scrolls the page, and the scroll stops when the fingers
   lift. It does not drift on.
7. A Bluetooth keyboard types into the host, with modifiers.
8. The on-screen keyboard types too, including a character that no key on the
   host's keyboard makes.
9. The host's sound plays on the phone, and the phone's volume keys change it.
10. The screen does not blank during the session.
11. A device paired with `--view-only` sees the desktop and cannot type or
    click. Built: a view-only session has no handle to the injector. Not yet
    run on a device.
12. Nothing runs on the host but the daemon.

## The numbers

Taken during that run, not from a synthetic pipeline.

- **Glass to glass:** how long from a change on the host until it shows on the
  phone. This is the number the project exists to reduce. Each stage that adds
  to it is reported separately, so that it is clear which one to attack.
- **Per stage:** capture, encode, network one way, decode, present.
- **Frame counts:** captured, encoded, sent, received and shown, so that a frame
  lost anywhere is attributed, not merely absent.

What the code reports now:

- Latency: the median capture-to-wire time (in the host's heartbeat and on the
  `tachyx-hostd status` row), the round trip (quinn's own estimate), and decode
  and present latency on the client.
- Counts: frames the host sent, gated and refused (in its heartbeat); frames
  delivered, dropped, missing and undecodable after reassembly on the client;
  frames pushed, decoded, presented and superseded on the client's present path.
- Not reported: a capture count, the one-way network time, and glass to glass.
  The last two need the two clocks related (`client-ffi/api/session.rs:165-171`).

## A degraded link

The same run, with the link degraded on purpose.

- The picture stays watchable as the bandwidth falls. The ladder spends
  bitrate, then frame rate, before it spends resolution.
- Bandwidth that comes back is used again, without flapping between two rungs.
- A lost frame does not leave the picture broken. Parity rebuilds it, or the
  client asks and a key frame repairs it.
- Key frames do not come so often that they become the congestion.
- A return path that goes silent costs a stall, not the session. **Not met on
  phone Wi-Fi** ([`findings.md`](findings.md), "The video path stalls, and the
  cause is a link this system overdrives").

## Not in this milestone

Coordination server, accounts and billing. Clipboard. More than one client at a
time. Clients other than Linux and Android. Hosts on compositors that are not
wlroots, and any host setup other than the test machine (see
[`assumptions.md`](assumptions.md)). A sandboxed capture and encode process.
Moving a relayed session to a direct path. Direct libva encode.
