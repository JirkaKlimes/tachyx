# Where this goes next

Written at the end of the first day, when the pipeline works end to end and the
picture on the phone is real but not yet good.

The comparison worth making is not against Sunshine and Moonlight as products.
They have audio, a clipboard, five client platforms, four codecs and a decade of
per-device workarounds, and none of that is interesting to build. The comparison
worth making is on latency and on what happens when the link is not perfect,
because that is what this project is for.

## What is already better, and why it cannot easily be copied

The transport is QUIC. Encryption, connection identity and path management come
with it, and a session survives moving between Wi-Fi and a mobile network. The
prior art runs its own reliability layer over raw datagrams and cannot adopt this
without breaking every client it already has.

Capture is `ext-image-copy-capture` straight into a shared buffer the encoder
reads. The portal route the alternatives take is itself a screencopy client
republishing frames through PipeWire, so it is the same path with more hops.
Presentation timestamps arrive four hundredths of a millisecond behind the copy.

Congestion is measured rather than assumed. Sunshine encodes at whatever bitrate
the client asked for; Parsec published that it deliberately does not control the
rate at all. On a stable local network that is defensible. On a phone's hotspot it
is not, and that is the case this project cares about.

Parity follows measured loss instead of sitting at a fixed fifth of the stream
forever. On a clean link that fifth is waste; on a bad one it is not enough.

## What is worse, and how much of it is permanent

Almost none of it.

Recovery is the real gap. This encoder offers neither intra refresh nor long-term
reference frames, so a single lost frame costs a full key frame, and a key frame
is eight times an ordinary one. Sunshine avoids this on one path only: reference
frame invalidation exists in its native NVENC encoder and nowhere else. Every
other encoder it supports logs that the feature is missing and sends a full key
frame, exactly as we do.

**But this is a limitation of the GStreamer element, not of the hardware.** VA-API
exposes an intra-refresh parameter that `vah264enc` does not surface, and
`cros-libva` reaches it. Going direct to libva would buy intra refresh, long-term
references, and a coded picture buffer that can actually be set to one frame
rather than the half second the element floors it at. That would put this ahead of
the prior art on AMD rather than behind it.

The rest of the list is work rather than architecture: audio, a clipboard, other
codecs, other platforms.

## In order

1. **Congestion control that works.** Written; needs to be proven on a real link
   rather than a shaped one, and its first observed behaviour was pathological:
   it drove the bitrate to the floor while holding full resolution.
2. **Direct libva encode.** The largest latency win available. Turns every
   recovery from a hundred and thirty kilobytes into one frame interval, and
   removes the coded picture buffer floor that currently costs half a second of
   reaction time.
3. **Adaptive parity, actually adapting.** The policy is written and tested and
   nothing calls it, because the receiver report only just exists.
4. **Pointer motion scaling.** The protocol says the host scales deltas by the
   ratio of capture to encode size. It does not, so the pointer moves at the
   wrong speed and the client works around it by measuring.
5. Then audio, which the architecture document calls the first complaint anyone
   will make, and a clipboard.

## The method that got this far

Nearly everything found today was found by running it, not by reading it. The
frame deadline that abandoned every key frame a third of the way through its own
arrival. The cursor session whose completion event only arrives once the mouse
moves, so it never fired on a machine where the daemon is started from the
keyboard. A bandwidth estimate reporting zero on a healthy hundred-megabit path. A
key frame interval counted in frames on a capture path that stops producing frames
when nothing moves.

None of those were reachable by reasoning about the code, and several contradicted
what the documentation of the thing in question said. `spec/findings.md` exists to
hold what measurement established, including where it overturned an earlier
measurement. Keep writing to it, and keep distrusting anything in it that was not
run.
