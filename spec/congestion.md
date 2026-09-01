# Congestion control

How the host decides how many bits a second to spend, and how it makes the
encoder spend that many.

Everything the previous design rested on was measured on loopback and on a
gigabit local network: 344 Mbit/s carried, no loss, a 193 microsecond round
trip. On that path there is nothing to control, so a controller that did
nothing looked correct. This document is what replaces it, and the reason it
exists is one measurement on a real link.

## The measurement that broke the old design

A Galaxy S25 over Wi-Fi 6, sixteen seconds into a session, from the client's own
overlay:

| | |
|---|---|
| Frames | 63 in, 1 dropped, 8 missing, 12 undecodable |
| Packets | 859 in, 24 lost, 40 late |
| Round trip | 13.91 ms |
| Stream | 2580x1080 at 74, 20.0 Mbit/s configured |
| Host | 64 sent, 179 gated |
| Link | lost, no heartbeat |

The host refused three frames out of four because its send queue was
permanently full. Twelve frames arrived and could not be decoded. Heartbeats
were late enough that the client declared the connection dead while it was
still carrying video.

None of that is a transport fault. It is what happens when the encoder is asked
for four times what the link carries. quinn's BBR reported up to 216 Mbit/s on
that link, so the estimate never constrained anything, and the only limit in the
system was the number the user configured.

## Why the transport's own estimate cannot be the bitrate

Two independent reasons, and either one on its own is fatal.

The first is that BBR measures delivery rate over a windowed maximum, and
802.11 delivers in aggregates. A key frame offered as ninety-two datagrams
leaves the host as one or two A-MPDUs and is acknowledged in a burst, so the
delivery interval BBR divides by is the aggregate's duration and not the link's.
The maximum filter then holds that figure for ten round trips. This is a known
pathology of BBR over Wi-Fi and it explains 216 Mbit/s on a path that would not
carry twenty. It is a mechanism, not a measurement: nothing here measured the
aggregation directly.

The second reason is decisive on its own. quinn exposes no per-packet
acknowledgement to the application. `Connection::on_ack_received` is private
(`quinn-proto-0.11.17/src/connection/mod.rs:1432`), the public `Event` enum
carries `HandshakeDataReady`, `Connected`, `ConnectionLost`, `Stream`,
`DatagramReceived` and `DatagramsUnblocked` and nothing else, and a datagram has
no application-visible identity after `send_datagram` returns. The only hook
that sees acknowledgement timing at all is a custom `congestion::Controller`,
and it is handed bytes and instants with no packet number and no way back to a
datagram.

So there is no way to learn what the path did to our video except to ask the
receiver. The feedback message below is not an optimisation. It is the only
source of information this system can have.

BBR stays in place. Its estimate is an upper bound that will not bind once the
sender paces below it, it still drives loss detection and recovery, and it stays
in the heartbeat as a diagnostic. It stops being the target.

## Choosing the estimator

`goog_cc` 0.1.4 is a direct port of libwebrtc's GoogCC. It is BSD licensed, has
no unsafe, depends only on `tracing`, and builds clean. It is 20,195 lines, of
which about 9,500 are not tests. It has one owner, five releases, and nothing
published since 2025-03-12, tracking a libwebrtc snapshot from December 2024.

The decisive fact is not the maintenance risk. It is that every algorithm in it
is private. `lib.rs` uses `mod x; use x::*;` rather than `pub mod`, so
`TrendlineEstimator`, `AimdRateControl`, `DelayBasedBwe`,
`AcknowledgedBitrateEstimator`, `ProbeController` and `ProbeBitrateEstimator`
are all unreachable; only their configuration structs are public. This was
checked by compiling against each one. The crate is therefore all or nothing:
you take `GoogCcNetworkController` behind `NetworkControllerInterface`, or you
take none of it.

Taking all of it means synthesising a WebRTC-shaped world. `SentPacket` wants
`prior_unacked_data`, `data_in_flight`, an `audio` flag and a `PacedPacketInfo`
carrying probe cluster identity. There are twelve interface methods covering
route changes, remote bitrate reports, stream configuration and network state
estimates, none of which apply here. Wiring that faithfully is most of the work,
and wiring it carelessly gives a controller that misbehaves in ways no
instrumentation can see, because the parts that would show the misbehaviour are
private.

What we actually need is small. The inter-arrival grouper is about sixty lines,
the trendline estimator about a hundred and twenty, the AIMD control about a
hundred and fifty, the link capacity estimator forty, the acknowledged rate
window forty, the loss backstop forty, the probe controller a hundred and
twenty, and the probe estimator fifty. Roughly six hundred lines of arithmetic
against nine and a half thousand lines we cannot open.

**We write it.** The crate stays useful in one specific way: it is on this
machine already, it carries libwebrtc's own unit tests, and
`GoogCcNetworkController` is drivable from a synthetic feedback trace. It
belongs in `dev-dependencies` as a conformance oracle, so our controller can be
run against the same trace as the reference and held to it. That buys the
confidence of a port without the maintenance of one, and nothing ships.

The parts we deliberately do not copy are `loss_based_bwe_v2` (3,943 lines, the
newest and least settled component upstream), the network state predictor, the
audio packet separation, the RTT-based backoff and route change handling.

## The estimator

Everything below runs on the host, once per receiver report, from packets in
send order. A packet whose send record has been discarded is skipped.

### Grouping

Packets are grouped the way libwebrtc groups them, because the delay signal is
taken between groups rather than between packets. A group is closed when a
packet's send time is more than 5 ms after the group's first send time. A group
is merged into its predecessor when the arrival delta is at most 5 ms and the
delay variation is negative, which is the signature of a burst arriving faster
than it was sent. A group is closed unconditionally after 100 ms.

For two consecutive completed groups, the delay variation is

    d = (arrival_last(i) - arrival_last(i-1)) - (send_last(i) - send_last(i-1))

in milliseconds. Positive means the group took longer to cross the path than it
took to leave, which is a queue filling.

### The trendline

The delay variations are accumulated, smoothed, and a least-squares slope is
taken over a window. This is the whole of the delay estimator:

    accumulated += d
    smoothed = 0.9 * smoothed + 0.1 * accumulated
    push (arrival_time - first_arrival_time, smoothed) into a window of 20
    trend = ordinary least squares slope of smoothed against arrival time

The slope is compared against an adaptive threshold, scaled so that a short
history cannot trigger a decision:

    modified = min(deltas_seen, 60) * trend * 4.0

`modified > threshold` is overuse, `modified < -threshold` is underuse, and
anything between is normal. Overuse is only reported once it has persisted more
than 10 ms and been seen more than once in a row, and only while the trend is
not falling. That triple condition is what stops a single late aggregate from
costing a backoff.

The threshold itself adapts, which is the part draft-ietf-rmcat-gcc-02 gets
wrong and which matters most on Wi-Fi, where the noise floor is not constant:

    if |modified| > threshold + 15: leave it alone
    k = 0.039 if |modified| < threshold else 0.0087
    threshold += k * (|modified| - threshold) * min(elapsed_ms, 100)
    threshold = clamp(threshold, 6, 600)

It starts at 12.5. It rises slowly when the link is noisy so that a noisy link
does not read as a congested one, and falls thirty times faster than it rises so
that a link which becomes quiet regains sensitivity quickly.

### The rate

An AIMD control turns the signal into a rate, and every one of its adjustments
is made against the **acknowledged** rate rather than against what we intended
to send. That distinction is the whole reason the controller is safe: it can
never talk itself into a number the path has not already carried.

The acknowledged rate is the arrival-stamped payload bytes over a sliding 500 ms
window. libwebrtc runs a Bayesian filter here instead. We do not, because the
report cadence already smooths at 50 ms and a filter whose behaviour we cannot
justify from a measurement is a filter we cannot tune. This is the one place we
knowingly deviate.

| Signal | Action |
|---|---|
| Overuse | `rate = 0.85 * acknowledged`, then hold |
| Normal after a hold | begin increasing |
| Underuse | hold |

While increasing, the step is multiplicative at 1.08 per second, capped so that
a single update can never claim more than one second of growth, and floored at
1000 bit/s. Once a link capacity estimate exists, the increase becomes additive
instead, at one average packet's worth per round trip plus 100 ms, floored at
4000 bit/s per second. The increase is bounded by `1.5 * acknowledged +
10 kbit/s` so that a quiet sender cannot climb away from what it has proven.

The link capacity estimate is an exponential average of the acknowledged rate at
each overuse, with a coefficient of 0.05, and of each probe result, with a
coefficient of 0.5. Its variance is tracked alongside and clamped to [0.4, 2.5],
and the estimate is reset whenever the acknowledged rate leaves three standard
deviations of it. Probes move it ten times faster than overuse does, which is
the point of probing.

### Loss

The delay estimator does not see loss that happens without a queue, which on
Wi-Fi is most of it. A second controller runs beside it on the cumulative
counters in each report, over a one second window:

| Loss | Action |
|---|---|
| Below 2% | increase by 8%, at most once a second |
| 2% to 10% | hold |
| Above 10% | `rate *= 1 - loss/2`, at most once every 300 ms |

The target is the lower of the two controllers. Worth noting against the
measurement above: 24 packets lost of 859 is 2.8%, which sits in the hold band.
The loss controller would have done nothing about the session that failed. The
delay signal has to carry this, and the loss controller is there for the link
that fails a different way.

### Bounds

The floor is 300 kbit/s. That is not arbitrary: the smallest rung of the ladder,
860x360 at 15 frames a second, needs 279 kbit/s at the quality figure below.
Under 300 kbit/s there is no picture worth sending, so the controller holds
there and lets loss speak rather than chasing a number that cannot be spent.

The ceiling is what the user and the client asked for, unchanged.

## The receiver report

The client says when each video datagram arrived. The key is the `seq` already
in the media header, which counts every datagram of the session and is compared
circularly in the manner of RFC 1982. Nothing new is needed to identify a
packet.

Added to `tachyx_proto::control`:

```rust
/// When each video datagram arrived, and which never did.
pub struct Feedback {
    /// The `seq` of the first datagram this report accounts for.
    pub base: Seq,
    /// The client's own clock at the arrival of the first datagram in
    /// `packets` that arrived, in 250-microsecond ticks since the client's
    /// session began.
    pub reference: u32,
    /// One entry per sequence number from `base` upward, in order, no gaps.
    pub packets: Vec<PacketArrival>,
    /// Datagrams the client has never seen, since the session began.
    pub lost: u64,
    /// Datagrams the client has received, since the session began.
    pub received: u64,
}

pub enum PacketArrival {
    /// Not received when the report was built.
    Missing,
    /// Received this many 250-microsecond ticks after the previous datagram in
    /// this report that arrived. Negative when the two were reordered. The
    /// first arrival is measured from `reference`, so its delta is zero.
    Received(i16),
}
```

`Seq` needs the serde derives; it does not have them today.

Deltas rather than offsets, because postcard varint-encodes and a delta between
consecutive datagrams is almost always one byte where an offset from the report
reference is two. Signed, because reordering is real: Wi-Fi retries and
multi-queue hardware both produce it, and an unsigned delta would have to lie
about it. Two hundred and fifty microsecond ticks because that is what
transport-wide feedback uses and because a signed 16-bit tick count then spans
8.19 seconds, which is thirty times the longest report interval.

**No clock synchronisation is required and none is implied.** Every quantity the
estimator takes from this message is a difference between two client arrival
times, so a constant offset between the two clocks cancels exactly. Rate skew
between the two crystals does not cancel, but at the parts-per-million a real
oscillator drifts it is far below the 6 ms threshold floor. This is why
`reference` is on the client's own clock and why `ClockSyncRequest` stays
unimplemented.

The cumulative `lost` and `received` counters are what the loss controller
reads. They are cumulative rather than per-interval so that a delayed report is
still exactly right about the interval it covers.

### How often

Every 50 ms, if at least one datagram arrived since the last report, and never
more than 512 statuses in one report. A report describing more than that is
split and the parts sent back to back.

libwebrtc computes the interval as 68 bytes of report against 5% of the estimate
and clamps it to [50 ms, 250 ms]. On any link above 218 kbit/s that formula
returns something below 50 ms and the clamp decides, so carrying the formula
would be carrying arithmetic with one answer. The single case where it does
something is a collapsing link, and one comparison reproduces it: while the
host's announced bitrate is under 250 kbit/s, report every 250 ms instead.

Measured, with real postcard encodings and the four byte length prefix the
control stream adds:

| Datagrams described | Bytes, no loss | Bytes, 5% loss | Bytes per datagram |
|---|---|---|---|
| 18 | 52 | 51 | 2.9 |
| 35 | 86 | 84 | 2.5 |
| 88 | 192 | 187 | 2.2 |
| 256 | 529 | 516 | 2.1 |
| 512 | 1041 | 1015 | 2.0 |

At 1418 byte payloads that is one report per 50 ms describing 8 datagrams at
2 Mbit/s, 35 at 8 Mbit/s and 88 at 20 Mbit/s. Adding thirty bytes for QUIC, UDP
and IP framing, the reports cost 9.8, 18.2 and 34.7 kbit/s, which is 0.49%,
0.23% and 0.17% of the video they describe. The cost falls as a share of the
link because the per-report overhead is fixed and the per-packet cost is two
bytes.

### The sender's side

The host keeps, for every datagram it releases, the sequence number, the release
time, the wire size, and the probe cluster it belonged to if any. Three seconds
of history at 20 Mbit/s is 5,289 entries, about 127 kB. Sequence numbers are
extended to 64 bits on the way in using `Seq::distance_from`, so a wrap every
93 seconds at 20 Mbit/s is invisible as long as reports keep arriving, and the
history is discarded and restarted if a report's base cannot be placed.

## The probe sender

Without probing the controller recovers by multiplicative increase alone, and
1.08 per second means a factor of five takes `ln 5 / ln 1.08`, which is 20.9
seconds. That is exactly the recovery time the earlier simulation observed after
an 80% capacity cut, and it is not a tuning problem. It is what the algorithm
does when nothing tests the link. Probing is not an optimisation of GCC. It is
half of it.

A probe is a cluster of datagrams released at a chosen rate, higher than the
current target, whose arrival spacing in the next report says whether the link
carried them. The result sets the estimate directly instead of waiting for the
increase to walk there.

### What a probe is made of

Media first. When a probe is scheduled and the encoder has just produced a
frame, that frame's datagrams are released at the probe rate rather than the
pacing rate, and they are the cluster. This costs nothing at all: the same bytes
were going out anyway, only sooner.

Padding second, because a desktop is usually still and produces no frames for
seconds at a time, which is precisely when a probe is most needed. A padding
datagram carries the ordinary 16 byte media header with **bit 2 of `flags`**
set, and a payload of zeroes. A receiver that sees that bit counts the sequence
number, records the arrival, and discards the packet without touching
reassembly: `frame_id`, `capture`, `slice_pkts`, `pkt_idx` and `slice_id` all
mean nothing on a padding packet. Sequence accounting is the only thing padding
participates in, and it is the only thing it exists for.

A cluster is `max(5 datagrams, 15 ms at the probe rate)`. The five packet floor
matters at low rates: at 300 kbit/s, 15 ms is one datagram and one datagram
measures nothing, so the cluster becomes five datagrams spread over 96 ms and
the spacing still reads the link.

### When to probe

At startup, immediately, at three times the start rate, and then at six times
once the first result is in. Thereafter at twice the current estimate whenever
the estimate has grown to within 70% of the last probe's rate, until the
ceiling is reached.

In steady state, whenever the sender has been application-limited for 5 seconds,
at twice the current estimate. A remote desktop is application-limited almost
all the time, because a still screen produces no frames, so this is the ordinary
case rather than the exception, and it is the specific answer to a controller
that would otherwise sit at a depressed rate forever after one transient.

After a fall, if the estimate drops below 66% of the highest estimate seen in
the last 5 seconds, probe at 85% of that high water mark.

A result is waited for at most 1 second, after which the cluster is abandoned.

The cost of the steady-state probe is fixed as a share of the link and does not
depend on the link. Probing at `2R` for `D` seconds every `T` seconds costs
`2D/T` of `R`, which at 15 ms every 5 seconds is 0.6%, whether R is 300 kbit/s
or 20 Mbit/s.

### Reading a probe

From the report, take the cluster's first and last send times and first and last
arrival times, and the bytes excluding the first packet in each direction:

    send_rate    = send_bytes / (last_send - first_send)
    receive_rate = receive_bytes / (last_arrival - first_arrival)

If `receive_rate / send_rate` exceeds 2.0 the cluster was not delivered as a
cluster and the result is discarded. Otherwise the result is
`min(send_rate, receive_rate)`, except that if `receive_rate` came in below 90%
of `send_rate` the link was saturated by the probe and the result is
`0.8 * receive_rate`, so that acting on it does not immediately overuse.

A result sets the AIMD control's rate outright and updates the link capacity
estimate at a coefficient of 0.5.

## Making the encoder produce less

This is where the old design was not merely mistuned but pointed at the wrong
thing. It set a number on the encoder and dropped whole frames after encoding
when the number did not work. Both halves are wrong.

### The bitrate property is not a limit, and it is barely a hint

Measured on this machine, `vah264enc` at 1920x1088, variable bitrate, declared
60 frames a second, eight seconds a run, output measured as bytes on disk:

| Content | Configured | Achieved | Ratio |
|---|---|---|---|
| snow | 1.0 Mbit/s | 11.54 Mbit/s | 11.5x |
| snow | 2.0 Mbit/s | 12.11 Mbit/s | 6.1x |
| snow | 4.0 Mbit/s | 12.93 Mbit/s | 3.2x |
| snow | 8.0 Mbit/s | 13.68 Mbit/s | 1.7x |
| snow | 16.0 Mbit/s | 18.99 Mbit/s | 1.2x |
| ball | 2.0 Mbit/s | 0.18 Mbit/s | 0.09x |
| ball | 8.0 Mbit/s | 0.19 Mbit/s | 0.02x |
| ball | 20.0 Mbit/s | 0.21 Mbit/s | 0.01x |

The property spans two orders of magnitude of error in each direction. On
content the encoder cannot compress it is pinned at its quantiser ceiling and
emits 24 kB a frame no matter what it is asked for. On content it can compress
it emits 400 bytes a frame no matter what it is asked for. Snow is a worst case
nobody's desktop reaches, but a video window playing at full screen is closer to
snow than to a still page, and the earlier finding that a bitrate of 20 produced
20.7 to 35.8 Mbit/s is the same effect at a milder amplitude.

The conclusion is unavoidable. **Nothing written into the encoder bounds what
goes on the wire.** The only thing that bounds it is refusing to give the
encoder pictures.

### The token bucket is the rate limiter

`tachyx_media::Encoder` already checks an `admission` flag before it pushes a
capture frame into the pipeline, and the pipeline is synchronous, so the byte
cost of a frame is known the moment it returns. That is everything a token
bucket needs.

    balance += target_bits * elapsed          (refilled continuously)
    balance = min(balance, ceiling)
    admit the next capture frame while balance >= 0
    balance -= encoded_bits                   (charged the real size)

The ceiling is the greater of 100 ms of the target and the current key frame
estimate, so a desktop that has been still for a fifth of a second has banked
enough credit to pay for a key frame outright. That is exactly the case that
matters: the user comes back, the screen changes, a key frame is needed, and it
must not be rationed at the moment the session becomes interactive again.

The balance may go negative by at most one frame, and the frames refused while
it recovers are refused **before** the encoder sees them. A frame never
submitted leaves the reference chain intact and costs the client nothing. That
is the entire difference between this and the gate that produced twelve
undecodable frames.

Charging the measured size rather than a modelled one is not fastidiousness. The
model is wrong. Measured, dropping capture frames ahead of the encoder at a
fixed declared frame rate:

| Frames delivered | Rate on the wire | Mean frame |
|---|---|---|
| 60.0/s | 13.68 Mbit/s | 28,491 B |
| 45.5/s | 11.06 Mbit/s | 30,374 B |
| 30.9/s | 8.43 Mbit/s | 34,124 B |
| 14.9/s | 5.56 Mbit/s | 46,702 B |

Halving the frame rate gives 62% of the bytes, not 50%, and quartering it gives
41%, not 25%. The wire rate follows roughly the 0.65 power of the delivered
frame rate, because the encoder's own rate control lowers its quantiser when it
finds itself under budget and spends the slack on the frames that do arrive.
This contradicts the line in [`findings.md`](findings.md) that says caps
declaring 60 while capture delivers 20 produces a third of the configured rate.
That reading is linear and the behaviour is not.

For the controller this is good news rather than bad: refused frames are partly
repaid as quality on the frames that survive. It only means the bucket must be
charged what actually happened.

### The coarse knob, and what it costs

The encoder's `bitrate` property still matters, because it sets the operating
point the bucket is asked to police. Set far too high, the bucket refuses most
frames and the picture judders. Set too low, the bucket refuses nothing and the
picture is soft for no reason.

Changing it costs a full key frame, and two traps make that worse than it
sounds.

The first is that the setter has no equality guard. Read from
`gst-plugins-bad/sys/va/gstvah264enc.c`, `PROP_BITRATE`, `PROP_CPB_SIZE` and
`PROP_TARGET_PERCENTAGE` all assign and then set `reconf` unconditionally, and
`gst_va_base_enc_handle_frame` turns `reconf` into a drain plus
`gst_va_base_enc_reset_state`, which restarts the group of pictures. Measured on
a running pipeline: writing 20000 to a `bitrate` that already held 20000
produced a key frame. **Anything that retargets the encoder must compare before
it writes.**

The second is the opposite, and it is free. Measured on the same pipeline, a
resolution change and a bitrate change applied in the same tick cost **one** key
frame between them, not two:

| Change | Key frames |
|---|---|
| bitrate 20000 to 6000 | one |
| size 3440x1440 to 1720x720 | one |
| size to 860x360 **and** bitrate to 2000 together | one |
| size to 3440x1440 **and** bitrate to 20000 together | one |
| bitrate 20000 to 20000 | one |

So the whole operating point moves in a single step, and the client sees one
`StreamConfig`. The retarget rule follows:

- Retarget down when the controller's target falls below 80% of what the encoder
  is set to.
- Retarget up when it rises above 130% of it.
- Never more than once a second, and always through the shared key frame budget
  in `idr.rs`.

Down is more eager than up because the two errors are not symmetric. Set 25%
high, the bucket refuses a fifth of frames and a 75 Hz stream becomes 60. Set
25% low, the picture is slightly soft and nothing else happens. Set twice too
high, half the frames go and the session is unusable.

### The coded picture buffer is not being set

A defect found while measuring, which belongs here because it decides how far
the encoder may overshoot before its rate control reacts.

`Control::apply_rate` sets `cpb-size` to half the bitrate in kilobits, which the
findings document describes as the 500 ms floor. It is not, because
`target-percentage` is 66. In `_ensure_rate_control`, variable bitrate sets
`max_bitrate = bitrate * 100 / target_percentage` and `target_bitrate =
bitrate`, and `_calculate_bitrate_hrd` rejects any `cpb_size` below
`max_bitrate / 2`, which is `0.758 * bitrate`, and silently substitutes
`max_bitrate * 2`. Confirmed in the log at 8000 kbit/s: `Too small cpb_size:
4000` followed by `Adjust cpb_size to: 24242`. The encoder is running with a
**two second** coded picture buffer, four times the intended figure and the
element's own default.

Setting `target-percentage` to 100 fixes both halves at once. `max_bitrate`
becomes `bitrate`, so `cpb-size = bitrate / 2` is exactly at the floor and is
honoured, giving the intended 500 ms. Measured, the change does not reintroduce
filler and does not lower the achieved rate: snow at 8000 kbit/s produced 12.62
Mbit/s at 66 and 13.68 Mbit/s at 100. It is not a ceiling and was never going to
be one, but it removes 1.5 seconds of accumulated slack that the encoder can
spend in a burst at the worst possible moment.

### The ladder becomes frame rate first, resolution last

The existing ladder in `ladder.rs` spends bitrate first and resolution last,
which is right for desktop content and stays. What it lacks is the rung between
them.

Frame admission drops whole capture frames, so the deliverable rates are the
output's refresh divided by a whole number. On a 74.983 Hz output that is 75,
37.5, 25, 18.75 and 15. A stride `n` in 1 to 5 sets both the declared frame rate
and the cadence the pump admits at, and 15 frames a second is the floor because
below it a desktop stops feeling connected.

Choosing the opening point needs a quality figure, and one is available from
measurement rather than assumption. The busiest desktop content measured on this
host, a busy quarter-screen window at 1080p60, carried 7.86 Mbit/s of picture,
which is 0.063 bits per pixel per frame. Rounding down, `q = 0.06`. The rule is
then: take the largest rung, and the smallest stride in 1 to 5, for which

    q * pixels * refresh / n <= target

Worked through at 74.983 Hz on a 3440x1440 capture:

| Target | Rung | Stride | Declared |
|---|---|---|---|
| 2 Mbit/s | 1720x720 | 3 | 25 fps |
| 8 Mbit/s | 3440x1440 | 3 | 25 fps |
| 20 Mbit/s | 3440x1440 | 2 | 37.5 fps |
| 25 Mbit/s | 3440x1440 | 1 | 75 fps |

Native resolution at 25 frames a second on 8 Mbit/s is the right answer for a
desktop and it is what the failed session should have produced. Sharp text at 25
Hz beats soft text at 75.

That rule is only used before anything has been measured, because `q` is a worst
case and real desktop content measures 0.006 to 0.063. After the first second
the ladder closes the loop on what the bucket is actually doing:

- Step down, raising the stride, or dropping a rung when the stride is already 5,
  when the bucket refuses more than 10% of offered frames over 2 seconds.
- Step up when the encoder's measured production over 5 seconds is under 60% of
  the target and the bucket has refused nothing.

Sixty percent because a step from stride `n` to `n-1` raises production by
roughly `n / (n - 1)`, which is 1.5 at the tightest step, so anything above
`1 / 1.5` of the target would step up into a refusal.

### The transport gate becomes what it was always for

`VideoSender::can_send_frame` stays exactly as it is. It does not exist for
congestion control and never did: it exists because quinn's datagram eviction
path double-subtracts and reaching it ends the connection. With the pacer below
keeping quinn's queue one or two datagrams deep, it will not fire, and that is
the point. It is a safety interlock against a bug, not a rate control mechanism,
and a session in which it fires is a session with something else wrong.

## Pacing

The controller's target is useless if the bytes leave in bursts. Three separate
things need pacing.

A key frame at 3440x1440 measured 192 kB. On an 8 Mbit/s link that is 192 ms of
link time. Handed to quinn all at once it fills the bottleneck queue and adds
192 ms of delay to everything behind it, including the heartbeat, which is
exactly how a live session came to report "no heartbeat".

The receiver report's send timestamps must be the times datagrams actually left,
and a deep queue in quinn makes them queue-entry times instead. A delay estimate
built on wrong send times is worse than none.

And control traffic shares packets with video, in an order we do not control.

So the host paces. Datagrams are released into quinn on a 1 ms tick from a token
bucket at `2.5 * target`, which is libwebrtc's pacing factor and exists so a
burst of media clears in a reasonable fraction of a frame interval without being
a burst on the wire. At 8 Mbit/s the release quantum is under 3 kB, so quinn's
queue holds one or two datagrams and the recorded send time is accurate to about
a millisecond. Anything the pacer has not released waits in the host's own
buffer, where we can see it, rather than in quinn's, where we cannot.

The pacer will not release while quinn holds more than four datagrams. If that
back pressure persists it means quinn's own congestion window has closed, which
is information, and it is reported rather than worked around.

`datagram_send_buffer_size` should shrink accordingly. It is currently sized to
hold a whole key frame, 203,785 bytes, because a key frame had to be handed over
whole. With a pacer in front of it there is no reason for quinn to hold more
than a handful of datagrams, and 64 KiB is generous. The receive buffer on the
client stays where it is: the client is not paced by us and a burst can still
arrive.

## Starting a session

A session begins with no estimate. Starting at the configured ceiling, which is
what happens today, is the single largest cause of the failure this document
opens with: 20 Mbit/s of demand on a link carrying eight, from the first frame,
with a 130 kB key frame sent as a burst before anything had measured anything.

**The session starts at 2.0 Mbit/s.** Not because 2 Mbit/s is a good guess about
the link, but because it is a rate that essentially every link this will run on
carries, and because a low start is cheap when probing makes it brief. libwebrtc
starts at 300 kbit/s for the same reason; a remote desktop can afford more
because it can afford to be wrong for a quarter of a second and cannot afford an
unreadable first picture.

At 2 Mbit/s the opening rule picks 1720x720 at a stride of 3, so 25 frames a
second. The first key frame at that size measured 38 kB, which is 155 ms at
2 Mbit/s and 52 ms at the first probe's rate.

The first key frame is the first probe. It has to be sent anyway, it is a
cluster of twenty-seven datagrams, and releasing it at 6 Mbit/s rather than
2 measures the link at three times the start rate for free. The second probe
follows at 12 Mbit/s, then doubling until the estimate stops keeping up or the
ceiling is reached, so on a 20 Mbit/s ceiling the sequence is 6, 12 and 20.

Each probe costs one report interval plus the cluster's own duration, so the
estimate reaches the truth in roughly 200 to 300 milliseconds. Without probing
it would take 20.9 seconds to climb from 2 to 20 Mbit/s. That difference is the
whole justification for building a probe sender.

## What the control stream needs

quinn writes DATAGRAM frames into a packet before STREAM frames, at
`quinn-proto-0.11.17/src/connection/mod.rs:3314` and `:3370` respectively, and
the datagram loop is unconditional: it drains the outgoing datagram queue into
every 1-RTT packet until the packet is full, and only then does
`write_stream_frames` get what is left. `SendStream::set_priority` does not help
and cannot: it sets the sort key of a heap of pending streams, so it orders
streams against other streams and has no relationship to datagrams at all.

There is exactly one transport-level knob, `datagram_send_buffer_size`, and it
is coarse. So the answer is not a priority setting. **The answer is that the
congestion controller must leave room and the pacer must keep the queue shallow,
and between them control never waits behind more than one packet of video.**

How much room, measured rather than guessed. Real postcard encodings plus the
four byte length prefix, plus thirty bytes of QUIC, UDP and IP framing:

| Message | Bytes | Rate | Cost |
|---|---|---|---|
| `Heartbeat` | 23 | 4/s | 1.7 kbit/s |
| `Feedback`, 88 packets | 192 | 20/s | 34.7 kbit/s |
| `StreamConfig` | 21 | rare | nothing |
| `KeyframeRequest` | 9 | rare | nothing |
| `InputBatch`, one event | 18 | up to 250/s | 96 kbit/s |
| `InputBatch`, four events | 54 | up to 250/s | 168 kbit/s |

Input dominates, and by a lot. `INPUT_TICK` is 4 ms, so a sustained drag can
produce 250 batches a second, and at one event a batch the message is 18 bytes
carrying about 12 bytes of pointer delta. On a phone reporting touches at the
display rate the real figure is nearer 120 batches a second and 46 kbit/s, but
the ceiling is what has to be reserved.

**The video target is the estimate less a flat 150 kbit/s.** Flat, not a
percentage, because control's cost does not scale with the video rate: 35
kbit/s of reports at the highest rate this system runs at, 2 kbit/s of
heartbeat, and up to 100 kbit/s of input. That is 7.5% of a 2 Mbit/s link and
0.75% of a 20 Mbit/s one, and both are the right answer.

Two things follow that are not congestion control but are caused by it. Input
batching at 4 ms spends 48 bytes on the wire to carry 12 bytes of pointer
motion, and on a starved link that ratio is worth revisiting. And the heartbeat
should carry the controller's own target rather than quinn's pacing rate, and
should count the bucket's refusals separately from the transport gate's, because
the overlay that reported "179 gated" could not say which mechanism did it.

## What this will not fix

Wi-Fi is not a queue. GCC assumes a bottleneck whose queue grows under load, and
802.11 aggregation, retries, rate adaptation and power-save naps all move delay
for reasons that are not congestion. The adaptive threshold absorbs the steady
part of that noise and the probe recovers from a false backoff within five
seconds, but false backoffs will happen and some of them will be visible.

It does not make loss survivable. 24 packets lost of 859 is 2.8%, and with one
reference frame and one slice per frame every lost packet destroys a frame and
every frame after it until the next key frame. Lowering the rate lowers how
often that happens; it does not change what happens when it does. The twelve
undecodable frames in the failed session came from post-encode drops and are
fixed by moving the drop ahead of the encoder. The ones that come from real wire
loss are not fixed by anything in this document. Forward error correction is
reserved in the header and not implemented, intra refresh and long-term
reference frames are unreachable on this hardware, and more slices measured
slower.

It does not escape the key frame tax. Every change to the operating point costs
a full key frame, and on a 2 Mbit/s link a 192 kB key frame is 768 ms of link
time. The budget in `idr.rs` therefore permits about one reconfiguration every
four seconds down there. Between reconfigurations the only knob is the token
bucket, so on a link that oscillates faster than the key frame budget the
picture judders rather than degrading smoothly. This is the largest single
limitation and it is hardware, not policy.

It does not drain somebody else's queue. If the access point is bufferbloated
and another device fills it, the delay signal reads it correctly and we back
off, which is right and does nothing for the latency, because the queue is not
ours.

It is not fair to TCP. GCC yields to delay and a file copy on the same network
does not, so a large download will take the link and we will sit low until it
finishes. That is GCC's known behaviour everywhere and we inherit it whole.

It measures one direction. Nothing estimates the upstream path that input and
reports travel on. A link asymmetric enough to congest upstream shows up as
reports going missing, and the controller holds, which is safe and not
diagnostic.

It does not help a client that is late for other reasons. Forty of the 859
packets were counted late, which is the client's own deadline being missed.
Lower rates and shallower queues reduce the causes; the client still needs a
policy for a packet that arrives after its frame is due.

And the sequence number is 16 bits, which wraps every 93 seconds at 20 Mbit/s.
The unwrapping is only correct while reports arrive more often than half a wrap.
They arrive every 50 milliseconds, so this is safe with three orders of
magnitude to spare, but a client that stops reporting for 45 seconds and resumes
is misread, and the history is discarded rather than trusted when a report's
base cannot be placed.

## Confidence

Measured on this machine, and reproducible from the scripts in the scratchpad:
the encoder's achieved rate against its configured rate on hard and easy content;
that dropping capture frames scales the wire rate as roughly the 0.65 power of
the frame rate; that `cpb-size` is silently replaced by a two second buffer under
`target-percentage` 66 and honoured under 100; that writing an unchanged bitrate
still costs a key frame; that a resolution change and a bitrate change applied
together cost one key frame; the key frame sizes at each rung; and every byte
count in the control stream table.

Read from source, not run: quinn's frame ordering and the scope of
`SendStream::set_priority`; that no per-datagram acknowledgement is exposed;
`goog_cc`'s size, dependencies and the privacy of its estimators, though the
privacy was confirmed by compiling against each type; and the VA encoder's rate
control arithmetic.

Taken from libwebrtc rather than measured here: every constant in the estimator
and the probe controller. They are the published values and they are known to
work on paths far worse than this one, but none of them has been tuned against
this hardware and several will need to be. The threshold floor of 6 ms is the
one most likely to be wrong on Wi-Fi.

Inferred, with the mechanism named but not measured: that BBR's 216 Mbit/s came
from 802.11 aggregation inflating its delivery rate filter. The number is real
and BBR being unusable as a target is established independently by the lack of
per-datagram acknowledgement, but the specific explanation is reasoning, not
evidence.

Guessed: the 2.0 Mbit/s start, the 150 kbit/s control reserve, the 100 ms bucket
ceiling, the 0.8 and 1.3 retarget band, the 10% and 60% ladder thresholds, and
the choice of 15 frames a second as the floor. Each is derived from something
measured, and each is the kind of number that only a session on a real link can
settle.
