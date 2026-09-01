//! The loss backstop.
//!
//! The delay estimator does not see loss that happens without a queue, which on
//! Wi-Fi is most of it. This runs beside it on the cumulative counters in each
//! report.
//!
//! Worth stating plainly against the session that provoked this whole design:
//! 24 packets lost of 859 is 2.8%, which sits in the hold band. This controller
//! would have done nothing about it. The delay signal has to carry that case,
//! and this is here for the link that fails a different way.

use std::collections::VecDeque;
use std::time::Duration;

use crate::history::micros_of;

/// Below this the rate climbs.
const LOW: f64 = 0.02;

/// Above this it falls.
const HIGH: f64 = 0.10;

/// How far the rate climbs, per step.
const GROWTH: f64 = 1.08;

/// Added to each climb so that a very low rate still moves.
const GROWTH_FLOOR: f64 = 1_000.0;

/// The shortest gap between two climbs, and the window the climb is taken
/// against the lowest rate over.
const INCREASE_INTERVAL: i64 = micros_of(Duration::from_secs(1));

/// The shortest gap between two falls.
const DECREASE_INTERVAL: i64 = micros_of(Duration::from_millis(300));

/// The counters of one report, as differences from the report before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Losses {
    /// Datagrams that never arrived over the interval.
    pub lost: u64,
    /// Datagrams that did.
    pub received: u64,
}

impl Losses {
    /// The share of the interval's datagrams that were lost.
    #[must_use]
    pub fn fraction(self) -> f64 {
        let total = self.lost + self.received;
        if total == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a datagram count over one report interval is a few hundred"
        )]
        {
            self.lost as f64 / total as f64
        }
    }
}

/// A rate that follows measured loss.
#[derive(Debug)]
pub(crate) struct LossControl {
    rate: f64,
    floor: f64,
    ceiling: f64,
    lowest: VecDeque<(i64, f64)>,
    increased_at: Option<i64>,
    decreased_at: Option<i64>,
    cumulative: Option<(u64, u64)>,
}

impl LossControl {
    pub(crate) fn new(start: f64, floor: f64, ceiling: f64) -> Self {
        Self {
            rate: start.clamp(floor, ceiling),
            floor,
            ceiling,
            lowest: VecDeque::new(),
            increased_at: None,
            decreased_at: None,
            cumulative: None,
        }
    }

    pub(crate) const fn rate(&self) -> f64 {
        self.rate
    }

    pub(crate) fn set_ceiling(&mut self, ceiling: f64) {
        self.ceiling = ceiling.max(self.floor);
        self.rate = self.rate.clamp(self.floor, self.ceiling);
    }

    /// Takes a probe result, which is a measurement of the link rather than a
    /// step towards one.
    pub(crate) fn probed(&mut self, rate: f64) {
        self.rate = rate.clamp(self.floor, self.ceiling);
        self.lowest.clear();
    }

    /// Applies the delay-based estimate as an upper limit.
    ///
    /// Without it the loss controller keeps a rate the delay signal has already
    /// refused, and hands it back the moment the delay estimate recovers, with
    /// nothing having re-proved it.
    pub(crate) fn lower_to(&mut self, rate: f64) {
        self.rate = self.rate.min(rate).clamp(self.floor, self.ceiling);
    }

    /// Takes one report's cumulative counters and returns the loss over the
    /// interval since the last report.
    ///
    /// The counters are cumulative rather than per interval so that a delayed
    /// report is still exactly right about the interval it covers.
    pub(crate) fn observe(&mut self, lost: u64, received: u64, now: i64) -> Losses {
        let (last_lost, last_received) = self.cumulative.unwrap_or((0, 0));
        // A counter that went backwards is a client that restarted its own
        // accounting; take the report as the new baseline rather than as an
        // enormous interval.
        let interval = if lost < last_lost || received < last_received {
            Losses::default()
        } else {
            Losses {
                lost: lost - last_lost,
                received: received - last_received,
            }
        };
        self.cumulative = Some((lost, received));
        self.remember(now);

        let fraction = interval.fraction();
        if fraction <= LOW {
            self.climb(now);
        } else if fraction > HIGH {
            self.fall(fraction, now);
        }
        self.rate = self.rate.clamp(self.floor, self.ceiling);
        interval
    }

    /// The climb is taken against the lowest rate of the last second rather
    /// than against the current one, so a fall is not immediately undone.
    fn remember(&mut self, now: i64) {
        while self
            .lowest
            .back()
            .is_some_and(|(_, rate)| *rate >= self.rate)
        {
            self.lowest.pop_back();
        }
        self.lowest.push_back((now, self.rate));
        let oldest = now - INCREASE_INTERVAL;
        while self.lowest.len() > 1 && self.lowest.front().is_some_and(|(at, _)| *at < oldest) {
            self.lowest.pop_front();
        }
    }

    fn climb(&mut self, now: i64) {
        if self
            .increased_at
            .is_some_and(|at| now - at < INCREASE_INTERVAL)
        {
            return;
        }
        let from = self.lowest.front().map_or(self.rate, |(_, rate)| *rate);
        self.rate = self.rate.max(GROWTH.mul_add(from, GROWTH_FLOOR));
        self.increased_at = Some(now);
    }

    fn fall(&mut self, fraction: f64, now: i64) {
        if self
            .decreased_at
            .is_some_and(|at| now - at < DECREASE_INTERVAL)
        {
            return;
        }
        self.rate *= 1.0 - fraction / 2.0;
        self.decreased_at = Some(now);
        self.increased_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: f64 = 300_000.0;
    const CEILING: f64 = 20_000_000.0;

    fn control() -> LossControl {
        LossControl::new(8_000_000.0, FLOOR, CEILING)
    }

    #[test]
    fn the_loss_that_broke_the_measured_session_sits_in_the_hold_band() {
        // 24 of 859, which is 2.8%. This controller is honest about doing
        // nothing here; the delay signal is what has to carry it.
        let mut loss = control();
        let mut now = 0;
        for report in 1..=40u64 {
            now += micros_of(Duration::from_millis(50));
            loss.observe(report * 24, report * 859, now);
        }
        assert!(
            (loss.rate() - 8_000_000.0).abs() < 1.0,
            "2.8% loss must neither raise nor lower the rate"
        );
    }

    #[test]
    fn heavy_loss_lowers_the_rate_and_no_faster_than_the_interval_allows() {
        let mut loss = control();
        let mut now = 0;
        let mut falls = 0;
        let mut held = loss.rate();
        for report in 1..=20u64 {
            now += micros_of(Duration::from_millis(50));
            // Twenty percent lost.
            loss.observe(report * 20, report * 80, now);
            if loss.rate() < held {
                falls += 1;
                held = loss.rate();
            }
        }
        assert!(
            loss.rate() < 8_000_000.0 * 0.7,
            "twenty percent loss must bite"
        );
        assert!(
            falls <= 4,
            "one second of reports allows at most four falls, and took {falls}"
        );
    }

    #[test]
    fn a_clean_link_climbs_at_most_once_a_second() {
        let mut loss = LossControl::new(1_000_000.0, FLOOR, CEILING);
        let mut now = 0;
        let mut climbs = 0;
        let mut held = loss.rate();
        for report in 1..=100u64 {
            now += micros_of(Duration::from_millis(50));
            loss.observe(0, report * 100, now);
            if loss.rate() > held {
                climbs += 1;
                held = loss.rate();
            }
        }
        assert!(
            climbs <= 6,
            "five seconds of reports climbed {climbs} times"
        );
        assert!(loss.rate() > 1_000_000.0);
    }

    #[test]
    fn a_counter_that_restarts_is_taken_as_a_new_baseline() {
        let mut loss = control();
        loss.observe(1_000, 10_000, 0);
        let interval = loss.observe(0, 0, micros_of(Duration::from_millis(50)));
        assert_eq!(interval, Losses::default());
    }
}
