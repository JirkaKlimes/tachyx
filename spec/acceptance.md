# What done means

Each crate proves its own part, and every one of those proofs can pass while the
product is unusable. Capture can hit 120 frames a second, the encoder can write a
file that plays, the transport can move frames across a loopback, injection can
reach the compositor, and the phone can still show a smeared picture where taps
land in the wrong place.

So the milestone is not the sum of those. It is this, measured:

## The run

A phone on the same network as this laptop. The daemon started with no arguments
and no window. The app opened, the address typed once.

1. The desktop appears.
2. Moving a finger moves the pointer, and it feels like a trackpad rather than
   like a delay.
3. One finger taps and the thing under the pointer is clicked. Two fingers tap
   and the context menu opens. Three fingers tap and the keyboard appears, and
   again and it goes away.
4. Two fingers drag and the page scrolls, and stops when the fingers lift rather
   than drifting on.
5. A Bluetooth keyboard types into the remote machine, with modifiers.
6. The on-screen keyboard types too, including a character no keyboard on this
   host has a key for.
7. The screen does not blank during the session.
8. Nothing is running on the host but the daemon.

## The numbers

Taken during that run, not from a synthetic pipeline.

- Glass to glass: something moves on the host, and how long until it moves on the
  phone. This is the number the project exists to reduce, and every stage that
  contributes to it is reported separately so it is clear which one to attack.
- Per stage: capture, encode, network one way, decode, present.
- Frames captured, encoded, sent, received and shown, so that a frame lost
  anywhere is attributable rather than merely absent.

## The link is not always good

The same run, with the link degraded on purpose.

- The picture stays watchable as the available bandwidth falls, and the ladder
  spends bitrate before it spends resolution.
- Recovering bandwidth is used again, without flapping between two rungs.
- A dropped frame does not leave the picture broken until the next scheduled key
  frame.
- Key frames do not arrive so often that they become the congestion.

## Not in this milestone

Pairing, authentication, relays, coordination, billing, audio, clipboard, more
than one client at a time, and any host that is not this one.
