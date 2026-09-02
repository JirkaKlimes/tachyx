//! Turning the delay signal into a rate.
//!
//! Every adjustment is made against the **acknowledged** rate rather than
//! against what the host intended to send. That distinction is the whole reason
//! the controller is safe: it can never talk itself into a number the path has
//! not already carried.

use std::collections::VecDeque;
use std::time::Duration;

use crate::delay::Signal;
use crate::history::micros_of;

/// How long the acknowledged rate is averaged over.
const ACKED_WINDOW: i64 = micros_of(Duration::from_millis(500));

/// The shortest span the acknowledged rate is reported over.
///
/// Without it, a frame's twelve datagrams arriving over one millisecond read as
/// a hundred megabits, and the controller would raise its target on the
/// strength of a burst the path merely absorbed.
const ACKED_MIN_SPAN: i64 = micros_of(Duration::from_millis(100));

/// What the rate is multiplied by on overuse.
const BACKOFF: f64 = 0.85;

/// The multiplicative increase, per second, while nothing is known about the
/// link's capacity.
const GROWTH_PER_SECOND: f64 = 1.08;

/// The smallest multiplicative step, so that a very low rate still climbs.
const MIN_MULTIPLICATIVE_STEP: f64 = 1_000.0;

/// The smallest additive climb, in bits per second per second.
const MIN_ADDITIVE_RATE: f64 = 4_000.0;

/// The packet size the additive increase is derived from.
const PACKET_BYTES: f64 = 1_200.0;

/// The frame interval the additive increase is derived from.
const FRAME_INTERVAL: f64 = 1.0 / 30.0;

/// Added to the round trip when working out how long the link takes to answer a
/// change, which is what paces the additive increase.
const RESPONSE_LAG: Duration = Duration::from_millis(100);

/// The round trip assumed until the transport reports one.
const DEFAULT_RTT: Duration = Duration::from_millis(200);

/// How far above the acknowledged rate the target may ever be set.
///
/// A quiet sender cannot climb away from what it has proven.
const OVERSHOOT: f64 = 1.5;
const OVERSHOOT_HEADROOM: f64 = 10_000.0;

/// The exponential coefficient the link capacity estimate moves by on overuse.
const CAPACITY_FROM_OVERUSE: f64 = 0.05;

/// The coefficient a probe result moves it by: ten times faster, which is the
/// point of probing.
const CAPACITY_FROM_PROBE: f64 = 0.5;

/// Where the variance of the capacity estimate starts, and the band it is held
/// in.
const DEVIATION_START: f64 = 0.4;
const DEVIATION_MIN: f64 = 0.4;
const DEVIATION_MAX: f64 = 2.5;

/// The rate the arrivals prove the path carried, over a sliding window.
///
/// libwebrtc runs a Bayesian filter here instead. We do not, because the report
/// cadence already smooths at 50 ms and a filter whose behaviour we cannot
/// justify from a measurement is a filter we cannot tune. This is the one place
/// this controller knowingly deviates from the reference.
#[derive(Debug, Default)]
pub(crate) struct Acknowledged {
    window: VecDeque<(i64, u64)>,
    bytes: u64,
}

impl Acknowledged {
    /// Takes one arrival, stamped on the client's clock.
    pub(crate) fn observe(&mut self, arrived: i64, wire: u64) {
        self.window.push_back((arrived, wire));
        self.bytes += wire;
        let oldest = arrived.saturating_sub(ACKED_WINDOW);
        while self
            .window
            .front()
            .is_some_and(|(at, _)| *at < oldest && self.window.len() > 1)
        {
            let (_, bytes) = self.window.pop_front().expect("checked a moment ago");
            self.bytes -= bytes;
        }
    }

    /// Bits a second the path is known to have carried, or `None` while the
    /// window is too short to say.
    pub(crate) fn rate(&self) -> Option<f64> {
        let (first, first_bytes) = self.window.front().copied()?;
        let (last, _) = self.window.back().copied()?;
        let span = last - first;
        if span < ACKED_MIN_SPAN {
            return None;
        }
        // The first arrival's bytes crossed the path before the window opened,
        // so the interval carries every byte but its own.
        let carried = self.bytes.saturating_sub(first_bytes);
        Some(bits_per_second(carried, span))
    }
}

/// An exponential average of what the link was carrying each time it was
/// pushed too hard, and of each probe result.
#[derive(Debug, Default)]
struct Capacity {
    estimate: Option<f64>,
    deviation: f64,
}

impl Capacity {
    fn update(&mut self, sample: f64, alpha: f64) {
        let estimate = match self.estimate {
            None => sample,
            Some(held) => (1.0 - alpha) * held + alpha * sample,
        };
        self.estimate = Some(estimate);
        let error = estimate - sample;
        let deviation = if self.deviation == 0.0 {
            DEVIATION_START
        } else {
            self.deviation
        };
        self.deviation = ((1.0 - alpha) * deviation + alpha * error * error / estimate.max(1.0))
            .clamp(DEVIATION_MIN, DEVIATION_MAX);
    }

    fn spread(&self) -> f64 {
        self.estimate.map_or(0.0, |estimate| {
            3.0 * (self.deviation.max(DEVIATION_START) * estimate).sqrt()
        })
    }

    fn upper(&self) -> f64 {
        self.estimate
            .map_or(f64::INFINITY, |estimate| estimate + self.spread())
    }

    fn lower(&self) -> f64 {
        self.estimate
            .map_or(0.0, |estimate| (estimate - self.spread()).max(0.0))
    }

    fn reset(&mut self) {
        self.estimate = None;
    }
}

/// What the control is doing between reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hold,
    Increase,
    Decrease,
}

/// The additive-increase, multiplicative-decrease control.
#[derive(Debug)]
pub(crate) struct Aimd {
    phase: Phase,
    rate: f64,
    floor: f64,
    ceiling: f64,
    capacity: Capacity,
    latest_acked: Option<f64>,
    changed_at: Option<i64>,
    rtt: Duration,
}

impl Aimd {
    pub(crate) fn new(start: f64, floor: f64, ceiling: f64) -> Self {
        Self {
            phase: Phase::Hold,
            rate: start.clamp(floor, ceiling),
            floor,
            ceiling,
            capacity: Capacity::default(),
            // Seeded with the start rate rather than left empty, so that a
            // session on a link nothing has yet measured still climbs. The
            // climb is bounded by this same figure, so the most an unmeasured
            // link is ever asked for is half again the start rate.
            latest_acked: Some(start.clamp(floor, ceiling)),
            changed_at: None,
            rtt: DEFAULT_RTT,
        }
    }

    #[cfg(test)]
    pub(crate) const fn rate(&self) -> f64 {
        self.rate
    }

    pub(crate) const fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub(crate) fn set_ceiling(&mut self, ceiling: f64) {
        self.ceiling = ceiling.max(self.floor);
        self.rate = self.rate.clamp(self.floor, self.ceiling);
    }

    /// Lowers the rate to at most `rate`, never raising it, and holds.
    ///
    /// For the controller decaying a blind estimate: the control has proven
    /// nothing, so it may only give ground. Holding, rather than keeping the old
    /// phase, means the first report to resume takes the ordinary hold to
    /// increase transition and dates its climb from that report, not from a
    /// change made while the link was silent.
    pub(crate) fn lower_to(&mut self, rate: f64) {
        self.rate = self.rate.min(rate).clamp(self.floor, self.ceiling);
        self.phase = Phase::Hold;
    }

    /// Takes a probe result, which sets the rate outright instead of waiting
    /// for the increase to walk there.
    pub(crate) fn probed(&mut self, rate: f64, now: i64) {
        self.capacity.update(rate, CAPACITY_FROM_PROBE);
        self.rate = rate.clamp(self.floor, self.ceiling);
        self.phase = Phase::Hold;
        self.changed_at = Some(now);
    }

    /// Takes one report's worth of signal and returns the rate to send at.
    pub(crate) fn observe(&mut self, signal: Signal, acked: Option<f64>, now: i64) -> f64 {
        if let Some(acked) = acked {
            self.latest_acked = Some(acked);
        }
        let acked = acked.or(self.latest_acked);
        self.phase = match (self.phase, signal) {
            (_, Signal::Overuse) => Phase::Decrease,
            (_, Signal::Underuse) => Phase::Hold,
            (Phase::Hold, Signal::Normal) => {
                self.changed_at = Some(now);
                Phase::Increase
            }
            (held, Signal::Normal) => held,
        };

        match self.phase {
            Phase::Hold => {}
            Phase::Increase => self.increase(acked, now),
            Phase::Decrease => self.decrease(acked, now),
        }
        self.rate = self.rate.clamp(self.floor, self.ceiling);
        self.rate
    }

    fn increase(&mut self, acked: Option<f64>, now: i64) {
        let elapsed = elapsed_seconds(self.changed_at, now);
        self.changed_at = Some(now);
        let Some(acked) = acked else { return };
        if acked > self.capacity.upper() {
            // The path is carrying more than the estimate says it can, so the
            // estimate is about a link that is no longer there.
            self.capacity.reset();
        }
        let limit = OVERSHOOT.mul_add(acked, OVERSHOOT_HEADROOM);
        if self.rate >= limit {
            return;
        }
        let step = if self.capacity.estimate.is_some() {
            self.additive(elapsed)
        } else {
            let alpha = GROWTH_PER_SECOND.powf(elapsed.min(1.0));
            (self.rate * (alpha - 1.0)).max(MIN_MULTIPLICATIVE_STEP)
        };
        self.rate = (self.rate + step).min(limit);
    }

    /// One average packet's worth per round trip plus a hundred milliseconds,
    /// which is roughly how long the link takes to show that a change happened.
    fn additive(&self, elapsed: f64) -> f64 {
        let frame_bits = self.rate * FRAME_INTERVAL;
        let packets = (frame_bits / (PACKET_BYTES * 8.0)).ceil().max(1.0);
        let packet_bits = frame_bits / packets;
        let response = 2.0 * (self.rtt + RESPONSE_LAG).as_secs_f64();
        (packet_bits / response).max(MIN_ADDITIVE_RATE) * elapsed
    }

    fn decrease(&mut self, acked: Option<f64>, now: i64) {
        self.changed_at = Some(now);
        self.phase = Phase::Hold;
        let Some(acked) = acked else { return };
        let mut lowered = BACKOFF * acked;
        if lowered > self.rate
            && let Some(capacity) = self.capacity.estimate
        {
            // The path carried more than we are sending, so the overuse is
            // about a queue somebody else filled. Fall to what the link was
            // last known to hold rather than to a number above the current one.
            lowered = BACKOFF * capacity;
        }
        if lowered < self.rate {
            self.rate = lowered;
        }
        if acked < self.capacity.lower() {
            self.capacity.reset();
        }
        self.capacity.update(acked, CAPACITY_FROM_OVERUSE);
    }
}

/// Seconds between two host clock readings, floored at zero.
fn elapsed_seconds(from: Option<i64>, to: i64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a session clock in microseconds is far below the exact integer range of a double"
    )]
    let micros = from.map_or(0, |from| (to - from).max(0)) as f64;
    micros / 1_000_000.0
}

/// Bits a second, for `bytes` carried over `span` microseconds.
#[expect(
    clippy::cast_precision_loss,
    reason = "byte counts and microsecond spans are far below the exact integer range of a double"
)]
pub(crate) fn bits_per_second(bytes: u64, span: i64) -> f64 {
    if span <= 0 {
        return 0.0;
    }
    (bytes as f64) * 8.0 * 1_000_000.0 / (span as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: f64 = 300_000.0;
    const CEILING: f64 = 20_000_000.0;

    #[test]
    fn a_link_nothing_has_measured_is_climbed_no_further_than_the_start_rate_proves() {
        let mut aimd = Aimd::new(2_000_000.0, FLOOR, CEILING);
        for tick in 0..400i64 {
            aimd.observe(Signal::Normal, None, tick * 50_000);
        }
        assert!(
            aimd.rate() <= 1.5 * 2_000_000.0 + 10_000.0 + 1.0,
            "with nothing acknowledged the climb is bounded by the start rate, and reached {}",
            aimd.rate()
        );
    }

    #[test]
    fn overuse_falls_to_a_fraction_of_what_the_path_carried() {
        let mut aimd = Aimd::new(8_000_000.0, FLOOR, CEILING);
        aimd.observe(Signal::Overuse, Some(6_000_000.0), 0);
        assert!(
            (aimd.rate() - 0.85 * 6_000_000.0).abs() < 1.0,
            "the backoff is taken against what arrived, not against what was asked for"
        );
    }

    #[test]
    fn the_rate_can_never_climb_away_from_what_the_path_has_proven() {
        let mut aimd = Aimd::new(1_000_000.0, FLOOR, CEILING);
        aimd.observe(Signal::Overuse, Some(1_000_000.0), 0);
        for tick in 1..2000i64 {
            aimd.observe(Signal::Normal, Some(1_000_000.0), tick * 50_000);
        }
        assert!(
            aimd.rate() <= 1.5 * 1_000_000.0 + 10_000.0 + 1.0,
            "a quiet sender climbed to {} on a link carrying one megabit",
            aimd.rate()
        );
    }

    #[test]
    fn a_probe_result_sets_the_rate_outright() {
        let mut aimd = Aimd::new(2_000_000.0, FLOOR, CEILING);
        aimd.probed(12_000_000.0, 0);
        assert!((aimd.rate() - 12_000_000.0).abs() < 1.0);
        // And it is remembered, so the next increase is additive rather than
        // the slow multiplicative walk.
        aimd.observe(Signal::Normal, Some(12_000_000.0), 100_000);
        aimd.observe(Signal::Normal, Some(12_000_000.0), 150_000);
        assert!(aimd.rate() > 12_000_000.0);
    }

    #[test]
    fn the_acknowledged_rate_ignores_a_burst_too_short_to_measure() {
        let mut acked = Acknowledged::default();
        for packet in 0..12i64 {
            acked.observe(packet * 100, 1434);
        }
        assert_eq!(acked.rate(), None, "1.2 ms of arrivals measures nothing");

        for packet in 0..600i64 {
            acked.observe(packet * 1_000, 1434);
        }
        let rate = acked.rate().expect("six hundred milliseconds of arrivals");
        assert!(
            (11_000_000.0..12_000_000.0).contains(&rate),
            "1434 bytes a millisecond is 11.5 Mbit/s, and this said {rate}"
        );
    }
}
