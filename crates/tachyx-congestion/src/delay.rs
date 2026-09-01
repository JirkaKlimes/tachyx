//! The delay signal: whether a queue on the path is filling, emptying, or
//! holding steady.
//!
//! Packets are grouped before they are compared, because the signal is taken
//! between groups rather than between packets: a sender that releases a frame's
//! datagrams back to back produces arrival deltas that say more about the
//! encoder than about the path. The delay variation between two groups is
//!
//! ```text
//! d = (arrival_last(i) - arrival_last(i-1)) - (send_last(i) - send_last(i-1))
//! ```
//!
//! Positive means the group took longer to cross the path than it took to
//! leave, which is a queue filling.
//!
//! Every constant here is libwebrtc's own. None has been tuned against this
//! hardware, and the threshold floor of 6 ms is the one most likely to be wrong
//! on Wi-Fi.

use std::collections::VecDeque;
use std::time::Duration;

use crate::history::micros_of;

/// A group is closed when a packet's send time is this far past the group's
/// first send time.
const GROUP_SPAN: i64 = micros_of(Duration::from_millis(5));

/// A group is merged into its predecessor when the arrival delta is at most
/// this and the delay variation is negative, which is the signature of a burst
/// arriving faster than it was sent.
const BURST_SPAN: i64 = micros_of(Duration::from_millis(5));

/// A group is closed unconditionally after this long, however the arrivals
/// look.
const MAX_BURST: i64 = micros_of(Duration::from_millis(100));

/// Consecutive groups that arrived out of order before the grouper gives up and
/// starts again.
const REORDER_RESET: u32 = 3;

/// Samples the least-squares slope is taken over.
const WINDOW: usize = 20;

/// How much of the accumulated delay each sample keeps.
const SMOOTHING: f64 = 0.9;

/// What the slope is multiplied by before it is compared with the threshold.
const GAIN: f64 = 4.0;

/// The cap on the sample count that scales the slope, so that a short history
/// cannot trigger a decision.
const MIN_DELTAS: f64 = 60.0;

/// The most deltas counted, past which the scaling is saturated anyway.
const MAX_DELTAS: u32 = 1000;

/// Where the adaptive threshold starts, in the slope's own units.
const THRESHOLD_START: f64 = 12.5;

/// How fast the threshold rises towards a noisy link.
const K_UP: f64 = 0.0087;

/// How fast it falls towards a quiet one: thirty times faster than it rises, so
/// a link that becomes quiet regains its sensitivity quickly.
const K_DOWN: f64 = 0.039;

/// Below this the picture would be unreadable anyway, and above it the detector
/// would never fire.
const THRESHOLD_MIN: f64 = 6.0;
const THRESHOLD_MAX: f64 = 600.0;

/// A reading this far past the threshold is not evidence about the noise floor,
/// so it does not move the threshold.
const MAX_ADAPT_OFFSET: f64 = 15.0;

/// The most elapsed time one threshold update may claim.
const MAX_ADAPT_SPAN_MS: f64 = 100.0;

/// How long overuse must persist before it is reported.
const OVERUSE_SPAN_MS: f64 = 10.0;

/// What the path is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Signal {
    /// The delay is not moving.
    #[default]
    Normal,
    /// A queue is filling.
    Overuse,
    /// A queue is draining, so the sender is below what the path carries.
    Underuse,
}

/// One closed group of packets.
#[derive(Debug, Clone, Copy)]
struct Group {
    first_send: i64,
    send: i64,
    first_arrival: i64,
    complete: i64,
}

/// Groups packets the way libwebrtc groups them and reports the delay variation
/// between consecutive groups.
#[derive(Debug, Default)]
struct Grouper {
    current: Option<Group>,
    previous: Option<Group>,
    reordered: u32,
}

/// The delay variation between two groups, in milliseconds, and when the later
/// one finished arriving.
#[derive(Debug, Clone, Copy)]
struct Variation {
    send_delta_ms: f64,
    delta_ms: f64,
    arrival: i64,
}

impl Grouper {
    fn observe(&mut self, sent: i64, arrived: i64) -> Option<Variation> {
        let Some(current) = self.current else {
            self.current = Some(Group {
                first_send: sent,
                send: sent,
                first_arrival: arrived,
                complete: arrived,
            });
            return None;
        };
        if sent < current.first_send {
            // Reordered on the way out, which the packetiser never does. It can
            // only mean the record was matched to the wrong datagram.
            return None;
        }

        let mut variation = None;
        if Self::opens_a_group(&current, sent, arrived) {
            if let Some(previous) = self.previous {
                let arrival_delta = current.complete - previous.complete;
                if arrival_delta < 0 {
                    self.reordered += 1;
                    if self.reordered >= REORDER_RESET {
                        *self = Self::default();
                    }
                    return None;
                }
                self.reordered = 0;
                let send_delta = current.send - previous.send;
                variation = Some(Variation {
                    send_delta_ms: as_ms(send_delta),
                    delta_ms: as_ms(arrival_delta - send_delta),
                    arrival: current.complete,
                });
            }
            self.previous = Some(current);
            self.current = Some(Group {
                first_send: sent,
                send: sent,
                first_arrival: arrived,
                complete: arrived,
            });
        }

        let group = self.current.as_mut().expect("filled a moment ago");
        group.send = group.send.max(sent);
        group.complete = arrived;
        variation
    }

    /// Whether this packet begins a new group rather than joining the open one.
    fn opens_a_group(current: &Group, sent: i64, arrived: i64) -> bool {
        !joins_the_burst(current, sent, arrived) && sent - current.first_send > GROUP_SPAN
    }
}

/// Whether this packet is part of a burst arriving faster than it was sent,
/// which is one group however its send times fall.
fn joins_the_burst(current: &Group, sent: i64, arrived: i64) -> bool {
    let send_delta = sent - current.send;
    if send_delta == 0 {
        return true;
    }
    let arrival_delta = arrived - current.complete;
    arrival_delta - send_delta < 0
        && arrival_delta <= BURST_SPAN
        && arrived - current.first_arrival < MAX_BURST
}

/// The delay estimator: a grouper, a smoothed accumulation of the variations,
/// and a least-squares slope compared against a threshold that adapts to the
/// link's own noise.
#[derive(Debug)]
pub(crate) struct Delay {
    groups: Grouper,
    accumulated: f64,
    smoothed: f64,
    first_arrival: Option<i64>,
    window: VecDeque<(f64, f64)>,
    deltas: u32,
    threshold: f64,
    last_update_ms: Option<f64>,
    trend: f64,
    previous_trend: f64,
    over_using_ms: Option<f64>,
    over_using_count: u32,
    signal: Signal,
}

impl Default for Delay {
    fn default() -> Self {
        Self {
            groups: Grouper::default(),
            accumulated: 0.0,
            smoothed: 0.0,
            first_arrival: None,
            window: VecDeque::with_capacity(WINDOW),
            deltas: 0,
            threshold: THRESHOLD_START,
            last_update_ms: None,
            trend: 0.0,
            previous_trend: 0.0,
            over_using_ms: None,
            over_using_count: 0,
            signal: Signal::Normal,
        }
    }
}

impl Delay {
    /// Takes one datagram the client said arrived, and reports what the path
    /// looks like now.
    pub(crate) fn observe(&mut self, sent: i64, arrived: i64) -> Signal {
        if let Some(variation) = self.groups.observe(sent, arrived) {
            self.push(variation);
        }
        self.signal
    }

    /// The threshold in force, which is the whole of what makes this usable on
    /// a link whose noise floor is not constant.
    #[cfg(test)]
    pub(crate) const fn threshold(&self) -> f64 {
        self.threshold
    }

    fn push(&mut self, variation: Variation) {
        self.deltas = (self.deltas + 1).min(MAX_DELTAS);
        let first = *self.first_arrival.get_or_insert(variation.arrival);
        self.accumulated += variation.delta_ms;
        self.smoothed = SMOOTHING * self.smoothed + (1.0 - SMOOTHING) * self.accumulated;
        self.window
            .push_back((as_ms(variation.arrival - first), self.smoothed));
        if self.window.len() > WINDOW {
            self.window.pop_front();
        }
        if self.window.len() == WINDOW {
            self.trend = slope(&self.window).unwrap_or(self.trend);
        }
        self.detect(variation);
    }

    fn detect(&mut self, variation: Variation) {
        if self.deltas < 2 {
            self.signal = Signal::Normal;
            return;
        }
        let modified = f64::from(self.deltas).min(MIN_DELTAS) * self.trend * GAIN;
        if modified > self.threshold {
            // The triple condition is what stops one late aggregate costing a
            // backoff: the overuse has to persist, be seen twice in a row, and
            // the trend must not already be falling.
            let over = self
                .over_using_ms
                .map_or(variation.send_delta_ms / 2.0, |held| {
                    held + variation.send_delta_ms
                });
            self.over_using_ms = Some(over);
            self.over_using_count += 1;
            if over > OVERUSE_SPAN_MS
                && self.over_using_count > 1
                && self.trend >= self.previous_trend
            {
                self.over_using_ms = Some(0.0);
                self.over_using_count = 0;
                self.signal = Signal::Overuse;
            }
        } else if modified < -self.threshold {
            self.over_using_ms = None;
            self.over_using_count = 0;
            self.signal = Signal::Underuse;
        } else {
            self.over_using_ms = None;
            self.over_using_count = 0;
            self.signal = Signal::Normal;
        }
        self.previous_trend = self.trend;
        self.adapt(modified.abs(), as_ms(variation.arrival));
    }

    /// Moves the threshold towards the reading, so that a noisy link does not
    /// read as a congested one.
    fn adapt(&mut self, modified: f64, now_ms: f64) {
        let last = *self.last_update_ms.get_or_insert(now_ms);
        if modified > self.threshold + MAX_ADAPT_OFFSET {
            self.last_update_ms = Some(now_ms);
            return;
        }
        let k = if modified < self.threshold {
            K_DOWN
        } else {
            K_UP
        };
        let span = (now_ms - last).clamp(0.0, MAX_ADAPT_SPAN_MS);
        self.threshold += k * (modified - self.threshold) * span;
        self.threshold = self.threshold.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
        self.last_update_ms = Some(now_ms);
    }
}

/// Ordinary least squares slope of the smoothed delay against arrival time.
fn slope(window: &VecDeque<(f64, f64)>) -> Option<f64> {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the window holds twenty samples"
    )]
    let count = window.len() as f64;
    let mean_x = window.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = window.iter().map(|(_, y)| y).sum::<f64>() / count;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (x, y) in window {
        numerator += (x - mean_x) * (y - mean_y);
        denominator += (x - mean_x) * (x - mean_x);
    }
    (denominator != 0.0).then(|| numerator / denominator)
}

/// Microseconds as milliseconds, which is the unit every constant above is in.
#[expect(
    clippy::cast_precision_loss,
    reason = "a session clock in microseconds is far below the exact integer range of a double"
)]
fn as_ms(micros: i64) -> f64 {
    micros as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link carrying everything it is offered: one frame of datagrams every
    /// sixteen milliseconds, arriving a constant delay later.
    fn steady(delay: &mut Delay, frames: i64) -> Signal {
        let mut signal = Signal::Normal;
        for frame in 0..frames {
            for packet in 0..12i64 {
                let sent = frame * 16_667 + packet * 120;
                signal = delay.observe(sent, sent + 13_910);
            }
        }
        signal
    }

    #[test]
    fn a_link_with_headroom_reads_as_normal() {
        let mut delay = Delay::default();
        assert_eq!(steady(&mut delay, 120), Signal::Normal);
    }

    #[test]
    fn a_queue_filling_is_detected() {
        let mut delay = Delay::default();
        steady(&mut delay, 60);
        // Every frame now arrives two milliseconds later than the one before,
        // which is a queue growing at 12% of the link.
        let mut signal = Signal::Normal;
        let mut queue = 0i64;
        for frame in 60..140i64 {
            queue += 2_000;
            for packet in 0..12i64 {
                let sent = frame * 16_667 + packet * 120;
                signal = delay.observe(sent, sent + 13_910 + queue);
            }
            if signal == Signal::Overuse {
                break;
            }
        }
        assert_eq!(signal, Signal::Overuse, "a growing queue must be seen");
    }

    #[test]
    fn a_queue_draining_reads_as_underuse() {
        let mut delay = Delay::default();
        // The link is clearing a backlog: every frame arrives three
        // milliseconds sooner than the one before.
        let mut queue = 900_000i64;
        let mut seen = false;
        for frame in 0..280i64 {
            for packet in 0..12i64 {
                let sent = frame * 16_667 + packet * 120;
                seen |= delay.observe(sent, sent + 13_910 + queue) == Signal::Underuse;
            }
            queue = (queue - 3_000).max(0);
        }
        assert!(seen, "a queue clearing must read as underuse");
    }

    #[test]
    fn a_noisy_link_raises_its_own_threshold() {
        // The part draft-ietf-rmcat-gcc-02 gets wrong, and the part that
        // matters most on Wi-Fi, where the noise floor is not constant: 802.11
        // aggregation, retries and power-save naps all move delay for reasons
        // that are not congestion. A fixed threshold reads that as a queue for
        // the whole session.
        let mut quiet = Delay::default();
        let mut quiet_peak: f64 = 0.0;
        for frame in 0..400i64 {
            for packet in 0..12i64 {
                let sent = frame * 16_667 + packet * 120;
                quiet.observe(sent, sent + 13_910);
            }
            quiet_peak = quiet_peak.max(quiet.threshold());
        }
        assert!(
            (quiet.threshold() - THRESHOLD_MIN).abs() < 0.5,
            "a link with no noise at all must fall to the floor, and sat at {}",
            quiet.threshold()
        );

        // Sixty milliseconds of delay swelling over a second and clearing over
        // the next, over and over.
        let mut noisy = Delay::default();
        let mut peak: f64 = 0.0;
        for frame in 0..480i64 {
            let phase = frame % 120;
            let swell = if phase < 60 { phase } else { 120 - phase } * 1_000;
            for packet in 0..12i64 {
                let sent = frame * 16_667 + packet * 120;
                noisy.observe(sent, sent + 13_910 + swell);
            }
            peak = peak.max(noisy.threshold());
        }
        // It falls thirty times faster than it rises, so it is back at the
        // floor between swells and the peak is what says it moved at all.
        assert!(
            peak > THRESHOLD_START && peak > quiet_peak,
            "the threshold must rise towards the link's own noise, and peaked at {peak} \
             against {quiet_peak} on a quiet link"
        );
    }

    #[test]
    fn a_single_late_aggregate_does_not_cost_a_backoff() {
        let mut delay = Delay::default();
        steady(&mut delay, 60);
        let mut signal = Signal::Normal;
        for packet in 0..12i64 {
            let sent = 60 * 16_667 + packet * 120;
            signal = delay.observe(sent, sent + 13_910 + 40_000);
        }
        assert_ne!(
            signal,
            Signal::Overuse,
            "one late aggregate is not congestion"
        );
    }
}
