# Congestion control

How the host decides how many bits a second to spend, and how it makes the
encoder spend that many.

The previous design was measured only on loopback and a gigabit LAN: 344 Mbit/s,
no loss, a 193 microsecond round trip. That path needs no control, so a
controller that did nothing looked correct.

## The failure

A Galaxy S25 on Wi-Fi 6, sixteen seconds into a session, from the client's
overlay:

| | |
|---|---|
| Frames | 63 in, 1 dropped, 8 missing, 12 undecodable |
| Packets | 859 in, 24 lost, 40 late |
| Round trip | 13.91 ms |
| Stream | 2580x1080 at 74, 20.0 Mbit/s configured |
| Host | 64 sent, 179 gated |
| Link | lost, no heartbeat |

The host refused three frames of four because its send queue was always full.
Twelve frames arrived and did not decode. Heartbeats came so late that the
client declared the link dead while video still flowed. The transport was not at
fault: the encoder was asked for four times what the link carried. quinn's BBR
reported up to 216 Mbit/s and never constrained anything, so the only limit was
the configured bitrate.

## Why the transport's estimate cannot be the target

Two independent reasons. Each is sufficient.

**BBR misreads the path.** Inferred: BBR keeps a windowed maximum of the
delivery rate, and 802.11 delivers in aggregates. A 92-datagram key frame leaves
as one or two A-MPDUs and is acknowledged in a burst. BBR divides by the
aggregate's duration, not the link's, and holds the result for ten round trips.
This known Wi-Fi pathology explains 216 Mbit/s on a path that does not carry
twenty. Measured, BBR also fails low: on a healthy 100 Mbit/s path it read
0.00 Mbit/s for every sample of a twenty-second run. It discards samples taken
while the sender is limited by its own supply, and a 60 fps sender is in that
state whenever an acknowledgement arrives.

**quinn exposes no per-packet acknowledgement.** Read from source, and decisive
alone. `Connection::on_ack_received` is private
(`quinn-proto-0.11.17/src/connection/mod.rs:1432`). The public `Event` enum has
only `HandshakeDataReady`, `Connected`, `ConnectionLost`, `Stream`,
`DatagramReceived` and `DatagramsUnblocked`. A datagram has no identity after
`send_datagram` returns. A custom `congestion::Controller` sees only bytes and
instants, with no packet number.

So only the receiver can say what the path did, and its report is the only path
information this system has. BBR stays for loss detection and recovery. Once the
host paces below it, its estimate is an upper bound that does not bind. Its
pacing rate stays in the heartbeat as a diagnostic.

## Choosing the estimator

`goog_cc` 0.1.4 is a direct port of libwebrtc's GoogCC: BSD licensed, no unsafe,
depends only on `tracing`, builds clean. It has 20,195 lines (about 9,500 not
tests), one owner, five releases, nothing since 2025-03-12, and tracks a
libwebrtc snapshot from December 2024.

Its privacy decides it, not its maintenance. `lib.rs` uses `mod x; use x::*;`,
not `pub mod`, so `TrendlineEstimator`, `AimdRateControl`, `DelayBasedBwe`,
`AcknowledgedBitrateEstimator`, `ProbeController` and `ProbeBitrateEstimator`
are unreachable. Only their configuration structs are public, as compiling
against each type confirmed. The only entry is `GoogCcNetworkController` behind
`NetworkControllerInterface`, which needs a faked WebRTC world. `SentPacket`
wants `prior_unacked_data`, `data_in_flight`, an `audio` flag and a
`PacedPacketInfo` with probe cluster identity. Twelve interface methods cover
route changes, remote bitrate reports, stream configuration and network state
estimates, and none apply. Wired carelessly, it misbehaves invisibly, because
the parts that would show the fault are private.

The parts needed come to about 600 lines: grouper 60, trendline 120, AIMD 150,
link capacity 40, acknowledged rate 40, loss backstop 40, probe controller 120,
probe estimator 50. Against 9,500 lines that cannot be opened, **we write it.**
`goog_cc` stays in `dev-dependencies` as a conformance oracle: it carries
libwebrtc's unit tests, and `GoogCcNetworkController` runs from a synthetic
feedback trace, so ours is held to the reference on the same trace. None of it
ships.

Not copied: `loss_based_bwe_v2` (3,943 lines, the newest and least settled part
upstream), the network state predictor, audio packet separation, RTT-based
backoff and route change handling.

## The estimator

The host runs it once per receiver report, over packets in send order. A packet
whose send record was discarded is skipped.

### Grouping

The delay signal is taken between groups, as in libwebrtc. A packet sent more
than 5 ms after the group's first send opens a new group. A packet whose arrival
delta is at most 5 ms and whose delay variation is negative joins the open
group: it is part of a burst that arrived faster than it was sent. A burst stops
joining 100 ms after the group's first arrival. Between two complete groups, in
milliseconds:

    d = (arrival_last(i) - arrival_last(i-1)) - (send_last(i) - send_last(i-1))

Positive `d` means the group took longer to cross the path than to leave, so a
queue is filling.

### The trendline

    accumulated += d
    smoothed = 0.9 * smoothed + 0.1 * accumulated
    push (arrival_time - first_arrival_time, smoothed) into a window of 20
    trend = ordinary least squares slope of smoothed against arrival time
    modified = min(deltas_seen, 60) * trend * 4.0

The `min(deltas_seen, 60)` factor stops a short history from deciding.
`modified > threshold` is overuse, `modified < -threshold` is underuse, anything
between is normal. Overuse counts only when it persisted more than 10 ms, was
seen more than once in a row, and the trend is not falling, so one late
aggregate cannot cause a backoff.

The threshold adapts. draft-ietf-rmcat-gcc-02 gets this wrong, and it matters
most on Wi-Fi, where the noise floor moves:

    if |modified| > threshold + 15: leave it alone
    k = 0.039 if |modified| < threshold else 0.0087
    threshold += k * (|modified| - threshold) * min(elapsed_ms, 100)
    threshold = clamp(threshold, 6, 600)

It starts at 12.5. It rises slowly, so noise does not read as congestion, and
falls about 4.5 times faster, so a link that becomes quiet regains sensitivity
quickly.

### The rate

An AIMD control turns the signal into a rate. It always adjusts against the
**acknowledged** rate, never against what the host meant to send, so it cannot
talk itself into a rate the path has not carried.

The acknowledged rate is the arrival-stamped wire bytes over a sliding 500 ms
window, once the window spans 100 ms (a frame that lands within 1 ms would read
as 100 Mbit/s). libwebrtc uses a Bayesian filter here. The 50 ms report cadence
already smooths, and a filter no measurement justifies cannot be tuned. This is
the estimator's one knowing deviation from libwebrtc.

| Signal | Action |
|---|---|
| Overuse | `rate = 0.85 * acknowledged` (never a rise), then hold |
| Normal after a hold | begin increasing |
| Underuse | hold |

- With no link capacity estimate, the increase is 1.08 per second, at most one
  second of growth per update, at least 1000 bit/s.
- With one, it is one average packet per `2 * (rtt + 100 ms)`, at least
  4000 bit/s per second. The average packet is a 30 fps frame at the current
  rate in 1200-byte packets. The round trip comes from quinn every 250 ms, and
  is 200 ms until known.
- The rate never exceeds `1.5 * acknowledged + 10 kbit/s`, so a quiet sender
  cannot climb past what it has proven. The start rate stands in until the
  first measurement.

The link capacity estimate is an exponential average of the acknowledged rate at
each overuse (coefficient 0.05) and of each probe result (0.5, ten times faster,
which is the point of probing). Its variance is clamped to [0.4, 2.5], and it
resets when the acknowledged rate leaves three standard deviations of it.

### Loss

The delay estimator misses loss without a queue, which on Wi-Fi is most loss. A
second controller reads the report's cumulative counters and takes the loss
fraction since the previous report:

| Loss | Action |
|---|---|
| 2% or less | `rate = 1.08 * (lowest rate of the last second) + 1 kbit/s`, at most once a second |
| 2% to 10% | hold |
| Above 10% | `rate *= 1 - loss/2`, at most once every 300 ms |

The target is the lower of the two controllers. Each report caps the loss
controller at the AIMD rate, and a probe result sets both.

The failed session lost 24 of 859 packets, 2.8%, which is in the hold band. The
loss controller would not have acted, so the delay signal must carry that case.
The loss controller is for a link that fails differently.

### Bounds

The floor is 300 kbit/s. The smallest rung, 860x360 at 15 frames a second, needs
279 kbit/s at the opening quality figure. Below that no picture is worth
sending, so the controller holds there and lets loss act.

The ceiling is the lower of the host's `max_bitrate` and the client's
`bitrate_ceiling`, and at least the floor. Each viewport change reads it again.

## The receiver report

The client reports when each video datagram arrived, keyed on the `seq` already
in the media header. `seq` counts every datagram of the session and compares
circularly (RFC 1982), so no new identifier is needed.

| `tachyx_proto::control::Feedback` | |
|---|---|
| `base: Seq` | `seq` of the first datagram the report covers |
| `reference: u32` | client clock at the first arrival in `packets`, in 250 microsecond ticks since the client's session began |
| `packets: Vec<PacketArrival>` | one entry per `seq` from `base` upward, in order, no gaps |
| `lost: u64` | datagrams never seen, since the session began |
| `received: u64` | datagrams received, since the session began |

`PacketArrival` is `Missing`, or `Received(i16)`: ticks since the previous
arrival in the report, negative when reordered, and zero for the first arrival,
which `reference` dates.

- **Deltas, not offsets.** postcard varint-encodes, so a delta is almost always
  one byte where an offset is two.
- **Signed.** Wi-Fi retries and multi-queue hardware reorder packets, and an
  unsigned delta would misreport that.
- **250 microsecond ticks.** Transport-wide feedback uses them, and a signed
  16-bit count then spans 8.19 seconds, about thirty times the longest report
  interval.

**No clock synchronisation is required and none is implied.** The estimator
uses only differences between client arrival times, so a constant clock offset
cancels. Crystal rate skew does not cancel, but at the parts per million a real
oscillator drifts it is far below the 6 ms threshold floor. So `reference` is on
the client's clock, and `ClockSyncRequest` stays unimplemented.

`lost` and `received` are cumulative, so a late report is still exact about its
interval. A slot that leaves the client's report window empty is lost. The
reassembler's counters are the wrong source: they call a packet lost when a
later one arrives and never retract. Measured on a phone, they counted 211 lost
while 429 arrived late, and held the session at 2 Mbit/s for the whole run.

### Cadence

Every 50 ms, if at least one datagram arrived since the last report. At most 512
statuses per report. A longer report splits into parts sent back to back.

libwebrtc sets the interval from 68 bytes of report against 5% of the estimate,
clamped to [50 ms, 250 ms]. Above 218 kbit/s the clamp decides. One comparison
reproduces the only other case, a collapsing link: while the host's announced
bitrate is under 250 kbit/s, the client reports every 250 ms.

Measured, with real postcard encodings and the control stream's 4-byte length
prefix:

| Datagrams described | Bytes, no loss | Bytes, 5% loss | Bytes per datagram |
|---|---|---|---|
| 18 | 52 | 51 | 2.9 |
| 35 | 86 | 84 | 2.5 |
| 88 | 192 | 187 | 2.2 |
| 256 | 529 | 516 | 2.1 |
| 512 | 1041 | 1015 | 2.0 |

At 1418-byte payloads a 50 ms report covers 8 datagrams at 2 Mbit/s, 35 at 8 and
88 at 20. With 30 bytes of QUIC, UDP and IP framing that is 9.8, 18.2 and
34.7 kbit/s: 0.49%, 0.23% and 0.17% of the video described. The overhead per
report is fixed and each packet adds about two bytes, so the share falls as the
rate rises.

### The sender's record

For each released datagram the host keeps the `seq`, release time, wire size and
probe cluster, if any. Three seconds at 20 Mbit/s is 5,289 entries, about
127 kB. `Seq::distance_from` extends `seq` to 64 bits on the way in, so the
wrap (every 37 seconds at 20 Mbit/s, 93 seconds at 8 Mbit/s) is invisible while
reports arrive. A report whose base cannot be placed discards the history.

## The probe sender

Without probes, recovery is multiplicative increase alone. At 1.08 per second a
factor of five takes `ln 5 / ln 1.08`, which is 20.9 seconds, and a simulation
observed exactly that after an 80% capacity cut. That is not a tuning problem.
It is what the algorithm does when nothing tests the link. Probing is half of
GCC.

A probe is a cluster of datagrams released above the current target. Their
arrival spacing in the next report shows whether the link carried that rate, and
the result sets the estimate directly.

### What a probe is made of

**Media first.** If a probe is due when the encoder has just produced a frame,
that frame's datagrams are the cluster, released at the probe rate instead of
the pacing rate. The bytes were leaving anyway, so this costs nothing.

**Padding second.** A still desktop produces no frames for seconds, which is
when a probe is most needed. A padding datagram is the normal 16-byte media
header with **bit 2 of `flags`** set, and a zero payload. The receiver counts its
`seq`, records its arrival and drops it before reassembly. `frame_id`,
`capture`, `slice_pkts`, `pkt_idx` and `slice_id` mean nothing on it.

A cluster is `max(5 datagrams, 15 ms at the probe rate)`. At 300 kbit/s, 15 ms
is one datagram, which measures nothing, so the cluster becomes five datagrams
over 96 ms.

### When to probe

- **Startup.** At once, at three times the start rate. Then at twice the
  estimate (six times the start rate after the first result) each time the
  estimate reaches 70% of the last probe's rate, up to the ceiling.
- **Steady state.** At twice the estimate whenever the sender has been
  application-limited for 5 seconds. A still screen produces no frames, so this
  is the ordinary case. It stops one transient from leaving the controller at a
  depressed rate for ever. (The build drops this gate. See the end.)
- **After a fall.** When the estimate drops below 66% of the highest estimate of
  the last 5 seconds, at 85% of that high-water mark.

One cluster at a time, at least 1 second apart, never during overuse. Both
steady-state rules wait 5 seconds after the last probe. A result not back within
1 second abandons the cluster, and its rate is forgotten.

Probing at `2R` for `D` seconds every `T` seconds costs `2D/T` of `R`. At 15 ms
every 5 seconds that is 0.6%, at 300 kbit/s or at 20 Mbit/s.

### Reading a probe

Take the cluster's first and last send times and first and last arrival times.
The send bytes exclude the last datagram sent, and the receive bytes exclude the
first datagram received.

    send_rate    = send_bytes / (last_send - first_send)
    receive_rate = receive_bytes / (last_arrival - first_arrival)

At least 4 datagrams must arrive. If `receive_rate / send_rate` exceeds 2.0, the
cluster was not delivered as a cluster and the result is discarded. Otherwise the
result is `min(send_rate, receive_rate)`. If `receive_rate` is below 90% of
`send_rate`, the probe saturated the link, and the result is
`0.8 * receive_rate` so that acting on it does not overuse at once. libwebrtc
uses 0.95. Here an overshoot costs a key frame, about eight ordinary frames, so
the factor is 0.8.

A result sets the AIMD rate outright and moves the capacity estimate with
coefficient 0.5.

## Making the encoder produce less

The old design set a number on the encoder, then dropped whole frames after
encoding when the number did not hold. Both halves were wrong.

### The bitrate property does not bound the output

Measured: `vah264enc` at 1920x1088, variable bitrate, 60 fps declared, eight
seconds a run, bytes on disk.

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

The error spans two orders of magnitude each way. On content it cannot
compress, the encoder sits at its quantiser ceiling and emits 24 kB a frame
whatever it is asked for. On easy content it emits 400 bytes a frame. No desktop
reaches snow, but a full-screen video window is closer to snow than to a still
page, and [`findings.md`](findings.md) records 20.7 to 35.8 Mbit/s at a
configured 20.

**Nothing written into the encoder bounds what goes on the wire.** Only refusing
to give it pictures does.

### The token bucket is the rate limiter

The encoder pump checks each capture frame before it enters the pipeline. The
pipeline is synchronous, so a frame's byte cost is known when the call returns:

    balance += target_bits * elapsed          (refilled continuously)
    balance = min(balance, ceiling)
    admit the next capture frame while balance >= 0
    balance -= encoded_bits                   (charged the real size)

The ceiling is the greater of 100 ms of the target and the last key frame, so a
desktop still for a fifth of a second can pay for a key frame outright. Then
someone returns, the screen changes, and the key frame is not rationed as the
session becomes interactive again.

The balance goes at most one frame negative. Frames refused while it recovers
are refused **before** the encoder sees them, so the reference chain stays
intact and the client loses nothing. That is the difference from the gate that
produced twelve undecodable frames.

The bucket charges the measured size because the model is wrong. Measured,
dropping capture frames ahead of the encoder at a fixed declared rate:

| Frames delivered | Rate on the wire | Mean frame |
|---|---|---|
| 60.0/s | 13.68 Mbit/s | 28,491 B |
| 45.5/s | 11.06 Mbit/s | 30,374 B |
| 30.9/s | 8.43 Mbit/s | 34,124 B |
| 14.9/s | 5.56 Mbit/s | 46,702 B |

Half the frame rate gives 62% of the bytes, not 50%. A quarter gives 41%, not
25%. The wire rate follows roughly the 0.65 power of the delivered frame rate:
under budget, the encoder lowers its quantiser and spends the slack on the
frames that arrive. This contradicts the linear reading in
[`findings.md`](findings.md), that caps declaring 60 while capture delivers 20
produce a third of the configured rate. It helps the controller, because refused
frames come back partly as quality.

### Retargeting the encoder

The `bitrate` property still sets the operating point the bucket polices. Far
too high, the bucket refuses most frames and the picture judders. Too low, the
picture is soft for no reason. Every change costs a full key frame.

**The setter has no equality guard.** Read from
`gst-plugins-bad/sys/va/gstvah264enc.c`: `PROP_BITRATE`, `PROP_CPB_SIZE` and
`PROP_TARGET_PERCENTAGE` assign and set `reconf` unconditionally, and
`gst_va_base_enc_handle_frame` turns `reconf` into a drain plus
`gst_va_base_enc_reset_state`, which restarts the group of pictures. Measured on
a running pipeline, writing 20000 to a `bitrate` that held 20000 produced a key
frame. **Anything that retargets the encoder must compare before it writes.**

**Changes in one tick share one key frame.** Measured on the same pipeline:

| Change | Key frames |
|---|---|
| bitrate 20000 to 6000 | one |
| size 3440x1440 to 1720x720 | one |
| size to 860x360 **and** bitrate to 2000 together | one |
| size to 3440x1440 **and** bitrate to 20000 together | one |
| bitrate 20000 to 20000 | one |

So the whole operating point moves in one step, and the client sees one
`StreamConfig`. The ladder retargets down when the controller's target falls
below 80% of the encoder's setting, and up when it rises above 130%, at most
once a second and always through the key frame budget in `idr.rs`.

Down is more eager because the errors are not symmetric. Set 25% high, the
bucket refuses a fifth of frames and 75 Hz becomes 60. Set 25% low, the picture
is slightly soft. Set twice too high, half the frames go and the session is
unusable.

### The coded picture buffer

The coded picture buffer sets how far the encoder may overshoot before its rate
control reacts. `Control::apply_rate` sets `cpb-size` to half the bitrate in
kilobits, 500 ms. At `target-percentage` 66 the element ignores it. For variable
bitrate, `_ensure_rate_control` sets
`max_bitrate = bitrate * 100 / target_percentage` and `target_bitrate = bitrate`.
`_calculate_bitrate_hrd` rejects any `cpb_size` below `max_bitrate / 2`, which is
`0.758 * bitrate`, and silently substitutes `max_bitrate * 2`. The log at
8000 kbit/s shows `Too small cpb_size: 4000`, then `Adjust cpb_size to: 24242`:
a **two second** buffer, four times the intent and the element's own default.

So the encoder sets `target-percentage` to 100. `max_bitrate` becomes `bitrate`,
and `cpb-size = bitrate / 2` sits exactly at the floor and is honoured.
Measured, no filler returns and the achieved rate does not drop: snow at
8000 kbit/s produced 12.62 Mbit/s at 66 and 13.68 Mbit/s at 100. It is not a
ceiling, but it removes 1.5 seconds of slack the encoder could spend in one
burst at the worst moment.

### The ladder: frame rate first, resolution last

`ladder.rs` spends bitrate first, frame rate second and resolution last, because
sharp text degrades far more under a resample than under coarser quantisation.

Frame admission drops whole capture frames, so the deliverable rates are the
refresh divided by a whole number: on a 74.983 Hz output, 75, 37.5, 25, 18.75
and 15. A stride `n` from 1 to 5 sets both the declared rate and the pump's
admit cadence. 15 frames a second is the floor, because below it a desktop stops
feeling connected. The rungs are halvings of the capture (a quarter, a half,
full), because scaling to three quarters measured 9.8 ms, more than not scaling
(7.6 ms), while a halving costs 5.1 ms.

The busiest desktop content measured, a busy quarter-screen window at 1080p60,
carried 7.86 Mbit/s: 0.063 bits per pixel per frame. Rounded down, `q = 0.06`.
The session opens at the largest rung, and the smallest stride, for which

    q * pixels * refresh / n <= target

The worked table, at 74.983 Hz on a 3440x1440 capture:

| Target | Rung | Stride | Declared |
|---|---|---|---|
| 2 Mbit/s | 1720x720 | 3 | 25 fps |
| 8 Mbit/s | 3440x1440 | 3 | 25 fps |
| 20 Mbit/s | 3440x1440 | 2 | 37.5 fps |
| 25 Mbit/s | 3440x1440 | 1 | 75 fps |

The failed session should have opened at native resolution and 25 frames a
second. Sharp text at 25 Hz beats soft text at 75.

`q` is a worst case, and real desktop content measures 0.006 to 0.063, so the
rule sets only the opening point. Then the ladder follows the bucket:

- **Down** when the bucket refuses more than 10% of offered frames over
  2 seconds. Raise the stride, or at stride 5 drop a rung and return to stride 1.
- **Up** when production over 5 seconds is under 60% of the target and nothing
  was refused. Lower the stride, or at stride 1 climb a rung and land at
  stride 2, because four times the pixels at half the rate is twice the demand.
- The resolution changes at most once every 5 seconds.

Sixty percent because a step from stride `n` to `n - 1` raises production by
roughly `n / (n - 1)`, which is 1.5 at the tightest step, so anything above
`1 / 1.5` of the target would step up into a refusal.

### The transport gate

`VideoSender::can_send_frame` is not congestion control. quinn's datagram
eviction path double-subtracts, and reaching it ends the connection. The gate
keeps that path unreachable. With the pacer keeping quinn's queue shallow it does
not fire, and a session where it fires has something else wrong.

## Pacing

A target is useless if the bytes leave in bursts:

- A 3440x1440 key frame measured 192 kB, 192 ms at 8 Mbit/s. Handed over at
  once, it fills the bottleneck queue and delays everything behind it by 192 ms,
  the heartbeat included. That is how a live session reported "no heartbeat".
- The report's send times must be departure times. A deep quinn queue makes them
  queue-entry times, and a delay estimate built on wrong send times is worse
  than none.
- Control shares packets with video, in an order the host does not choose.

The pacer is in `crates/tachyx-transport/src/pace.rs` and `video.rs`, with the
measurements behind each bound:

- **Rate** `max(2.5 * target, picture wire bytes / 35 ms)`. 2.5 is libwebrtc's
  pacing factor: a media burst clears in a fraction of a frame interval without
  being a burst on the wire. The floor keeps a key frame inside the receiver's
  50 ms frame deadline. Measured, a key frame spread over 43.8 ms arrived 98 to
  99% of the time, and one spread over 52 ms never arrived.
- **Burst allowance**, the lesser of 25,000 bytes and 15 ms at the pacing rate,
  so a delta frame leaves unpaced.
- **Per datagram.** Each datagram is charged when released. Waits under 1 ms are
  not slept, and the debt carries forward.
- **Shallow quinn queue.** Before each picture and after each wait, the sender
  waits until quinn's queue holds at most 2 ms at the configured rate, and at
  most 50 ms per picture. Unsent datagrams wait in the host, where they are
  visible. The estimator groups over 5 ms, so residence that varies by less
  cannot move the signal.

Nothing paces what reaches the client, so its receive buffer stays large.

## Starting a session

A session begins with no estimate. Starting at the configured ceiling is the
single largest cause of the failure this document opens with: 20 Mbit/s of
demand from the first frame on a link that carries eight, and a 130 kB key frame
sent as a burst before anything was measured.

**The session starts at 2.0 Mbit/s.** Almost every link carries it, and probing
makes a low start brief. libwebrtc starts at 300 kbit/s for the same reason. A
remote desktop starts higher because being wrong for a quarter of a second is
cheap and an unreadable first picture is not.

At 2 Mbit/s the opening rule picks 1720x720 at stride 3, 25 frames a second. Its
first key frame measured 38 kB: 155 ms at 2 Mbit/s, 52 ms at the first probe's
rate. That key frame, 27 datagrams that must go anyway, is the first probe, and
at 6 Mbit/s it measures three times the start rate for free. The probes then
double until the estimate stops keeping up or the ceiling is reached: 6, 12 and
20 on a 20 Mbit/s ceiling.

Each probe costs a report interval plus its own duration, so the estimate
arrives in about 200 to 300 ms. Multiplicative increase alone needs
`ln 10 / ln 1.08`, which is 29.9 seconds, from 2 to 20 Mbit/s.

## Room for the control stream

quinn writes DATAGRAM frames into a packet before STREAM frames
(`quinn-proto-0.11.17/src/connection/mod.rs:3314` and `:3370`). The datagram
loop fills every 1-RTT packet from the datagram queue unconditionally, and
`write_stream_frames` gets what is left. `SendStream::set_priority` sorts
pending streams against each other and has no relation to datagrams. The only
transport knob, `datagram_send_buffer_size`, is coarse. **So the controller
leaves room and the pacer keeps the queue shallow, and control never waits
behind more than one packet of video.**

Measured, with postcard encoding, the 4-byte length prefix, and 30 bytes of
QUIC, UDP and IP framing:

| Message | Bytes | Rate | Cost |
|---|---|---|---|
| `Heartbeat` | 23 | 4/s | 1.7 kbit/s |
| `Feedback`, 88 packets | 192 | 20/s | 34.7 kbit/s |
| `StreamConfig` | 21 | rare | nothing |
| `KeyframeRequest` | 9 | rare | nothing |
| `InputBatch`, one event | 18 | up to 250/s | 96 kbit/s |
| `InputBatch`, four events | 54 | up to 250/s | 168 kbit/s |

Input costs the most. `INPUT_TICK` is 4 ms, so a sustained drag sends up to 250
batches a second, each 18 bytes around about 12 bytes of pointer delta. A phone
touching at the display rate sends nearer 120 a second, 46 kbit/s, but the
reserve must cover the ceiling. On a starved link, 48 wire bytes per 12 bytes of
motion needs another look.

**The video target is the estimate less a flat 150 kbit/s.** Flat, because
control does not scale with video: 35 kbit/s of reports at the highest rate this
system runs at, 2 kbit/s of heartbeat, and up to 100 kbit/s of input. That is
7.5% of a 2 Mbit/s link and 0.75% of a 20 Mbit/s one, and both are right. While
sound flows, its 128 kbit/s plus 12.8 kbit/s of headers also comes off the video
share, never below the floor. Sound is neither paced nor gated, so on top of the
video the estimator would read it as the picture asking for too much.

The heartbeat carries the controller's `estimate` beside quinn's `pacing_rate`,
and counts the bucket's `frames_refused` apart from the gate's `frames_gated`.
The overlay that reported "179 gated" could not say which mechanism fired.

## What this does not fix

- **Wi-Fi is not a queue.** Aggregation, retries, rate adaptation and
  power-save naps move delay with no congestion. The adaptive threshold absorbs
  the steady noise and a probe undoes a false backoff within five seconds, but
  some false backoffs will be visible.
- **Rate does not make loss survivable.** With one reference frame and one slice
  per frame, a lost packet breaks every frame until the next key frame. A lower
  rate makes that rarer, not milder. The failed session's twelve undecodable
  frames came from post-encode drops, and refusing before the encoder removes
  those. Wire loss needs the parity in [`protocol.md`](protocol.md), which
  shares the queue, so the target must cover it too (the build does not, see
  the end). Intra refresh and long-term reference frames are unreachable on this
  hardware, and more slices measured slower.
- **The key frame tax stays.** On a 2 Mbit/s link a 192 kB key frame is 768 ms
  of link time, so `idr.rs` allows about one reconfiguration every four seconds.
  Between them only the bucket acts, so a link that oscillates faster judders.
  This is the largest limitation, and the hardware sets it, not policy.
- **Somebody else's queue stays full.** The delay signal reads a bufferbloated
  access point that another device fills, and the host backs off. That is
  right, and it does nothing for latency.
- **It is not fair to TCP.** GCC yields to delay and a file copy does not, so a
  large download takes the link until it finishes. This design inherits that.
- **It measures one direction.** Upstream congestion shows only as missing
  reports, and the controller holds. That is safe, not diagnostic. (The build
  decays after 6 seconds of silence, see the end.)
- **Late clients.** 40 of the 859 packets missed the client's own deadline.
  Shallower queues reduce the causes, but the client still needs a policy for a
  packet that arrives after its frame is due.
- **One path per session.** Route change handling was not copied, so the
  estimate, like quinn's window, is wrong once the path changes.
  [`relay.md`](relay.md) names this as the reason a relayed session is not moved
  onto a direct path.
- **A 16-bit sequence.** It wraps every 37 seconds at 20 Mbit/s, and unwrapping
  holds only while reports arrive within half a wrap, 18 seconds. At 50 ms the
  margin is over two orders of magnitude. A client silent for longer is
  misread, so the host discards history it cannot place.

## Confidence

**Measured** on the development host: the encoder's achieved rate against its
configured rate on hard and easy content; the 0.65-power scaling of wire rate
with delivered frame rate; `cpb-size` silently replaced by a two second buffer
under `target-percentage` 66 and honoured under 100; an unchanged bitrate write
costing a key frame; a resolution and a bitrate change together costing one; the
key frame sizes at each rung; every byte count in the report and control stream
tables.

**Read from source, not run:** quinn's frame ordering and the scope of
`SendStream::set_priority`; the missing per-datagram acknowledgement; `goog_cc`'s
size, dependencies and private estimators (the privacy also confirmed by
compiling against each type); the VA encoder's rate control arithmetic.

**Taken from libwebrtc, not measured here:** every constant in the estimator and
the probe controller except the 0.8 saturated-probe factor. They are the
published values and work on far worse paths. None has been tuned on this
hardware, and several will need it. The 6 ms threshold floor is the one most
likely to be wrong on Wi-Fi.

**Inferred**, mechanism named but not measured: that 802.11 aggregation
inflating BBR's delivery rate filter produced the 216 Mbit/s. The number is
real, and the missing per-datagram acknowledgement alone rules BBR out as a
target. The explanation is reasoning, not evidence.

**Guessed:** the 2.0 Mbit/s start, the 150 kbit/s control reserve, the 100 ms
bucket ceiling, the 0.8 and 1.3 retarget band, the 10% and 60% ladder
thresholds, and the choice of 15 frames a second as the floor. Each derives from
a measurement. Only sessions on real links can settle them.

## Where the build departs from the design

Recorded here as well as in code, so a reader who finds the two disagreeing can
tell a decision from a mistake.

**The steady-state probe is not gated on being application-limited.** It fires
whenever nothing has tested the link for 5 seconds (`IDLE_INTERVAL` in
`crates/tachyx-congestion/src/probe.rs`). The gate excludes the case where
recovery matters most: the link recovers while someone works, the encoder
produces exactly its target, and the controller stays at the depressed rate for
as long as the work goes on. Without the gate the cost is the same 0.6%.

`GoogCcNetworkController` cannot follow. `NetworkControllerInterface` cannot say
the sender is application-limited, because libwebrtc's own pacer supplies that
signal and is not behind the interface. Driven by a feedback trace alone, the
reference finds the link and gives it up exactly as ours does, and never takes
it back. The conformance test asserts the agreement and names the divergence.

**`datagram_send_buffer_size` does not shrink to 64 KiB.** The gate asks whether
a whole frame fits before the frame is encoded, so a buffer smaller than a key
frame refuses every key frame for ever. [`findings.md`](findings.md) records
that failure under "two frames". `video_send_buffer` in
`crates/tachyx-transport/src/video.rs` sizes it per session: the measured
130,427-byte 1080p key frame scaled by pixel count to the coded size, plus
parity at the 50% ceiling and per-datagram overhead, plus half again. Behind
the pacer it stays nearly empty, so the size costs nothing. A smaller buffer
needs a per-datagram gate, which could queue half a frame and refuse the rest,
and a half-sent frame is what the gate exists to prevent.

**A probe cluster is one contiguous burst.** The padding that completes a cluster
goes in the *same* burst as the picture that opened it, not behind the next
picture. Measured on a shaped 12 Mbit/s link, a cluster that straddled a frame
boundary spanned the idle gap, read 5.2 Mbit/s, and dragged the estimate down
every time. Contiguous, the same link reads 8.3 to 10.9 Mbit/s. The head picture
leaves at the higher of the probe rate and its own pacing rate, so it keeps its
deadline. The reader uses recorded send times, so the result still holds.

**The pacer is not a 1 ms tick.** The design released from a bucket at
`2.5 * target` on a 1 ms tick (under 3 kB a tick at 8 Mbit/s, so quinn held one
or two datagrams), and stopped while quinn held more than four. Back pressure
that persisted meant quinn's congestion window had closed, to be reported, not
worked around. The build is the pacer above. Its 35 ms floor keeps a key frame
inside the receiver's deadline, and its queue bound is in time, because a
datagram count is a different wait at every rate.

**A silent return path decays the estimate.** After 6 seconds with no usable
report, the estimate falls by a factor of 0.85 each 250 ms tick
(`SILENCE_THRESHOLD`, `SILENCE_DECAY` in `crates/tachyx-congestion/src/lib.rs`):
20 Mbit/s to the floor in under seven seconds. Six seconds is just above the
longest a living still desktop goes without feedback, the 5 second padding probe
plus one report interval. When pictures are offered and none reach the wire, the
stall watchdog (`Live::watch` in `crates/tachyx-hostd/src/session.rs`) drops the
estimate to the floor at once, clears the probe history and forces a key frame.

**Parity is not deducted from the encoder's share.** The bucket gets the
estimate less 150 kbit/s, less sound while it flows, and the packetiser adds
parity on top (`Live::video_share` in `crates/tachyx-hostd/src/session.rs`). The
estimator counts wire bytes, so heavy parity still reaches it through the delay
signal. On a loss-free link parity is zero after one second, and the two agree.
