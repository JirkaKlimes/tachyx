//! Releasing video at a byte rate instead of as fast as the transport takes it.
//!
//! quinn has a pacer of its own and it does nothing for us. It refills at
//! `1.25 * window / smoothed_rtt`, which with BBR's steady-state window gain is
//! about two and a half times the bandwidth estimate, and it allows two
//! milliseconds of burst on top of that. Measured on a path with a 13.91 ms
//! round trip: the ninety-two datagrams of a 1080p key frame reached the client
//! spread over **0.08 ms**, which is thirteen gigabits a second. At the rates
//! this sends, quinn's pacer never engages.
//!
//! What such a burst destroys is not the link's rate but the bottleneck's
//! buffer. Measured on a 100 Mbit/s path, frame delivery went 0.1, 12, 12, 19,
//! **97** and 98 percent as the bottleneck queue went 16, 32, 64, 96, **128**
//! and 192 kB. The step is at one key frame: 130,427 bytes offered in eighty
//! microseconds either fits the queue at the far end or does not, and the
//! link's rate never enters into it. We do not choose that queue, cannot see
//! it, and it lives inside an access point, so the transport must not depend on
//! its depth.
//!
//! Pacing over a fixed slice of the frame interval is the obvious design and it
//! is the wrong one. A frame is worth nothing until its last datagram, so
//! spreading every frame over nine tenths of an interval costs the whole slice:
//! measured, the median latency went from 9.21 ms to **23.64 ms**. Pacing at a
//! byte rate costs a small picture nothing and only a large one anything.
//! Measured on a 100 Mbit/s path with a 32 kB bottleneck queue, three seeds:
//! 51.7, 7.3 and 0.0 percent of frames delivered when bursting, **100 percent
//! on all three** when paced, for about one millisecond of median latency, and
//! the ninety-ninth percentile fell from 42.6 ms to 14.3 ms.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// The pacing rate, as a percentage of the bitrate the session is configured to
/// send.
///
/// It has to be above 100 or the pacer becomes the bottleneck, and the margin
/// is set by how far the encoder overshoots what it is asked for rather than by
/// taste. Measured on this hardware, `vah264enc` asked for 8 Mbit/s produced
/// 13.68 Mbit/s on content it could not compress, and asked for 20 Mbit/s
/// produced 20.7 to 35.8. Anything under about 180 percent would throttle the
/// encoder's own output on a full-screen video window.
///
/// It is not higher because the release rate is exactly what an unseen
/// bottleneck queue sees whenever the configured bitrate is above what the path
/// carries. 250 is libwebrtc's `kDefaultPaceMultiplier`, which is what it uses
/// before send-side feedback exists, and no feedback exists here yet: quinn's
/// own estimate was measured reporting 0.00 Mbit/s on a healthy 100 Mbit/s
/// path, for every sample of a twenty-second run.
pub const PACE_PERCENT: u32 = 250;

/// How long a whole picture may take to leave, however low the configured
/// bitrate is.
///
/// Pacing too slowly destroys frames as surely as bursting does, because the
/// receiver gives a frame 50 ms from its first datagram and then abandons it.
/// Measured on a shaped link: a key frame whose datagrams spanned 43.8 ms was
/// delivered 98 to 99 percent of the time, and one that spanned 52 ms was
/// delivered **0 percent**, every one timing out a millisecond or two before
/// its last datagram arrived. 35 ms is inside the first figure with room for
/// the jitter one air hop was measured to add: 8.3 percent of its packets
/// arrive more than 5 ms behind the fastest.
const FRAME_SPAN: Duration = Duration::from_millis(35);

/// The most the pacer ever releases at once.
///
/// The burst allowance is what keeps a small picture from being paced at all,
/// and it is also exactly the backlog the pacer is willing to put into a queue
/// it cannot see. 25,000 bytes is the largest picture measured to survive a
/// burst into the smallest bottleneck queue that survives anything: bursting
/// into 32 kB, a key frame capped at 25,000 bytes delivered **100 percent** of
/// frames and one capped at 40,000 delivered 6.2 percent.
const MAX_BURST: usize = 25_000;

/// The burst allowance, as time at the pacing rate.
///
/// Below about 13 Mbit/s this binds rather than [`MAX_BURST`], and it is what
/// keeps the allowance proportionate on a slow link, where 25,000 bytes would
/// be a large fraction of a second of link time.
///
/// It is set from the picture it has to cover. A delta frame is one frame
/// interval of the configured bitrate, so at [`PACE_PERCENT`] it is
/// `100 / PACE_PERCENT` of a frame interval at the pacing rate: 6.7 ms at
/// 60 Hz and 13.3 ms at 30, whatever the absolute rate. 15 ms therefore lets a
/// delta frame leave untouched at every rate this runs at, and a key frame,
/// eight times larger, feels the pacer at every one of them.
const BURST_SPAN: Duration = Duration::from_millis(15);

/// The rate to release video at, in bits a second, for a session configured to
/// send `bitrate` a picture that weighs `frame` bytes on the wire.
///
/// `frame` is what the packetiser produced, so it counts the per-datagram
/// header and any parity, not the encoded picture. Taking it from the encoded
/// length instead understates a key frame by about a third once parity is
/// carried, and the frame then takes half as long again to leave as the floor
/// below intends.
///
/// It is the size of the picture in hand rather than a remembered estimate of
/// the largest, because the floor only has to hold for the frame it is applied
/// to, and an estimate is wrong exactly at the moment that matters: the first
/// key frame of a session, before anything has been measured.
///
/// Not taken from the transport. `Connection::congestion_state` reports a
/// pacing rate only under BBR, and BBR's own bandwidth filter only records a
/// sample while the sender is not limited by its own supply. A sender that
/// fills quinn's queue in a hundred microseconds and then has nothing for
/// sixteen milliseconds is in that state whenever an acknowledgement arrives,
/// so on a link with headroom the estimate is never taken at all. Pacing at it
/// was measured collapsing an unbottlenecked stream to 45.8 percent of frames,
/// because the estimate falls, which slows the pacer, which makes the sender
/// more application limited, which lowers the estimate again.
///
/// The floor is not a refinement. Without it a key frame at a low configured
/// bitrate runs past the receiver's deadline and every one of them is thrown
/// away: at 8 Mbit/s configured, [`PACE_PERCENT`] alone spreads the measured
/// 130,427-byte key frame over 52 ms, which is the span measured delivering
/// nothing.
#[must_use]
pub fn rate_for(bitrate: NonZeroU32, frame: usize) -> NonZeroU32 {
    // Widened before the multiply, not after: 250 percent of any bitrate above
    // 17 Mbit/s overflows a `u32`, and saturating there and then dividing turns
    // the multiplier into a divider.
    let above = u64::from(bitrate.get()) * u64::from(PACE_PERCENT) / 100;
    let above = u32::try_from(above).unwrap_or(u32::MAX);
    NonZeroU32::new(above.max(rate_over(frame, FRAME_SPAN))).unwrap_or(bitrate)
}

/// A token bucket over bytes.
///
/// It is told the time rather than reading the clock, so the same sequence of
/// calls always produces the same schedule and a test needs no socket and no
/// sleeping.
///
/// The state is one instant: the time by which everything released so far will
/// have been paid for at the current rate. Bytes may leave up to one burst
/// allowance ahead of it, and credit for an idle spell does not accumulate past
/// that allowance.
#[derive(Debug, Clone)]
pub struct Pacer {
    rate: NonZeroU32,
    burst: Duration,
    paid: Instant,
}

impl Pacer {
    /// A bucket releasing `rate` bits a second, full at `now`.
    #[must_use]
    pub fn new(rate: NonZeroU32, now: Instant) -> Self {
        Self {
            rate,
            burst: burst_for(rate),
            paid: now,
        }
    }

    /// The rate in force, in bits a second.
    #[must_use]
    pub const fn rate(&self) -> NonZeroU32 {
        self.rate
    }

    /// Releases at `rate` from here on.
    ///
    /// Bytes already charged keep the cost they were charged at, so a rate
    /// change takes effect on the next byte rather than rewriting the debt.
    pub fn set_rate(&mut self, rate: NonZeroU32) {
        self.rate = rate;
        self.burst = burst_for(rate);
    }

    /// How long to wait before putting `bytes` on the wire.
    ///
    /// The bytes are charged whether the caller waits or not, so a caller that
    /// releases early borrows against its next release rather than escaping the
    /// rate.
    pub fn release(&mut self, bytes: usize, now: Instant) -> Duration {
        self.paid = self.paid.max(now);
        let due = self.paid.checked_sub(self.burst).unwrap_or(self.paid);
        self.paid = self
            .paid
            .checked_add(cost(bytes, self.rate))
            .unwrap_or(self.paid);
        due.saturating_duration_since(now)
    }
}

/// How long a burst allowance lasts at `rate`.
fn burst_for(rate: NonZeroU32) -> Duration {
    BURST_SPAN.min(cost(MAX_BURST, rate))
}

/// How long `rate` takes to carry `bytes`.
fn cost(bytes: usize, rate: NonZeroU32) -> Duration {
    let bits = u128::try_from(bytes).unwrap_or(u128::MAX).saturating_mul(8);
    let nanos = bits.saturating_mul(1_000_000_000) / u128::from(rate.get());
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// The rate that carries `bytes` in `span`, in bits a second.
fn rate_over(bytes: usize, span: Duration) -> u32 {
    let bits = u128::try_from(bytes).unwrap_or(u128::MAX).saturating_mul(8);
    u32::try_from(bits.saturating_mul(1_000_000_000) / span.as_nanos()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1080p60 key frame and a P frame from the same stream, as they go on
    /// the wire: the sizes measured on the target machine, cut into 1418-byte
    /// payloads under a sixteen-byte header.
    const KEY_FRAME: usize = 130_427 + 92 * 16;
    const DELTA_FRAME: usize = 16_673 + 12 * 16;
    const DATAGRAM: usize = 1418 + 16;

    const fn rate(bits: u32) -> NonZeroU32 {
        NonZeroU32::new(bits).expect("a rate")
    }

    /// Bytes `rate` carries in `span`, which is what a burst allowance held as
    /// a duration is worth.
    fn bytes_in(span: Duration, rate: NonZeroU32) -> usize {
        usize::try_from(span.as_nanos() * u128::from(rate.get()) / 8_000_000_000)
            .unwrap_or(usize::MAX)
    }

    /// Offers `bytes` a datagram at a time, all at `now`, and returns how long
    /// the last one is asked to wait.
    fn offer(pacer: &mut Pacer, bytes: usize, now: Instant) -> Duration {
        let mut left = bytes;
        let mut wait = Duration::ZERO;
        while left > 0 {
            let datagram = left.min(DATAGRAM);
            wait = pacer.release(datagram, now);
            left -= datagram;
        }
        wait
    }

    #[test]
    fn a_burst_is_spread() {
        let start = Instant::now();
        let mut pacer = Pacer::new(rate(20_000_000), start);
        let waited = offer(&mut pacer, KEY_FRAME, start);

        // Everything but the burst allowance and the last datagram leaves at
        // the rate: 131,899 bytes less 25,000 less 1,405 is 105,494, which is
        // 42.2 ms of a twenty megabit link.
        assert_eq!(bytes_in(pacer.burst, pacer.rate), 25_000);
        assert!(
            (Duration::from_millis(41)..Duration::from_millis(44)).contains(&waited),
            "a key frame at 20 Mbit/s spreads over about 42 ms, not {waited:?}"
        );
    }

    #[test]
    fn a_trickle_is_not_delayed() {
        let start = Instant::now();
        let mut pacer = Pacer::new(rate(20_000_000), start);
        for slot in 0..600u32 {
            let now = start + Duration::from_micros(16_667) * slot;
            assert_eq!(
                offer(&mut pacer, DELTA_FRAME, now),
                Duration::ZERO,
                "a delta frame every 16.67 ms never waits at 20 Mbit/s"
            );
        }
    }

    #[test]
    fn the_rate_is_honoured_over_time() {
        let start = Instant::now();
        let mut pacer = Pacer::new(rate(20_000_000), start);
        let mut now = start;
        let mut bytes = 0usize;
        for _ in 0..2000 {
            now += pacer.release(DATAGRAM, now);
            bytes += DATAGRAM;
        }

        let carried = rate_over(bytes, now - start);
        // Above the rate by the burst allowance, which is free and is 0.9% of
        // what this offered.
        assert!(
            (20_000_000..20_400_000).contains(&carried),
            "2000 datagrams left at {carried} bit/s against a rate of 20 Mbit/s"
        );
    }

    #[test]
    fn a_rate_change_takes_effect() {
        let start = Instant::now();
        let mut slow = Pacer::new(rate(10_000_000), start);
        let mut fast = Pacer::new(rate(10_000_000), start);
        fast.set_rate(rate(40_000_000));

        let slowly = offer(&mut slow, KEY_FRAME, start);
        let quickly = offer(&mut fast, KEY_FRAME, start);
        assert_eq!(fast.rate(), rate(40_000_000));
        assert!(
            quickly * 3 < slowly,
            "four times the rate carries a key frame in under a third of the \
             time, and took {quickly:?} against {slowly:?}"
        );
        // Above 13 Mbit/s the burst is bounded by bytes and below it by time.
        assert_eq!(slow.burst, BURST_SPAN);
        assert_eq!(bytes_in(fast.burst, fast.rate), MAX_BURST);
    }

    #[test]
    fn a_clock_that_jumps_does_not_break_the_schedule() {
        let start = Instant::now();
        let mut pacer = Pacer::new(rate(20_000_000), start);
        assert!(offer(&mut pacer, KEY_FRAME, start) > Duration::from_millis(40));

        // Far enough ahead that the debt is long paid: the bucket is full
        // again, and no more than full.
        let ahead = start + Duration::from_secs(60);
        assert_eq!(offer(&mut pacer, DELTA_FRAME, ahead), Duration::ZERO);
        assert!(offer(&mut pacer, KEY_FRAME, ahead) > Duration::from_millis(40));

        // A clock reading a minute earlier than the last one is answered with a
        // wait of about a minute rather than a panic, and the schedule is the
        // ordinary one again once time passes the debt.
        assert!(pacer.release(DATAGRAM, start) > Duration::from_secs(59));
        assert_eq!(
            offer(&mut pacer, DELTA_FRAME, ahead + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn the_multiplier_survives_a_bitrate_that_overflows_the_product() {
        // 250 percent of anything above 17.18 Mbit/s does not fit a `u32`, and
        // the ceiling a client announces is routinely 20.
        assert_eq!(rate_for(rate(20_000_000), 1), rate(50_000_000));
        assert_eq!(rate_for(NonZeroU32::MAX, 1), rate(u32::MAX));
    }

    #[test]
    fn a_delta_frame_leaves_at_once_and_a_key_frame_does_not() {
        // The two pictures measured on this machine, at the bitrate a session
        // carrying them is configured to send.
        let bitrate = rate(8_000_000);
        assert_eq!(
            rate_for(bitrate, DELTA_FRAME),
            rate(20_000_000),
            "a delta frame is small enough that the multiplier binds"
        );
        assert_eq!(
            rate_for(bitrate, KEY_FRAME),
            rate(30_148_342),
            "a key frame is large enough that the deadline floor binds"
        );

        let start = Instant::now();
        let mut pacer = Pacer::new(rate_for(bitrate, DELTA_FRAME), start);
        let mut now = start;
        for slot in 0..600u32 {
            let key = slot.is_multiple_of(60);
            let frame = if key { KEY_FRAME } else { DELTA_FRAME };
            now = now.max(start + Duration::from_micros(16_667) * slot);
            // As the send path does: every picture is paced at the rate its own
            // size on the wire asks for.
            pacer.set_rate(rate_for(bitrate, frame));
            let span = offer(&mut pacer, frame, now);
            if key {
                assert!(
                    (Duration::from_millis(25)..FRAME_SPAN).contains(&span),
                    "a key frame is spread, and inside the receiver's deadline: {span:?}"
                );
                now += span;
            } else if slot % 60 > 2 {
                // The two slots that fall inside a key frame's span pay off
                // what it borrowed. Every other delta frame leaves at once.
                assert_eq!(span, Duration::ZERO, "delta frame {slot} was paced");
            }
        }
    }
}
