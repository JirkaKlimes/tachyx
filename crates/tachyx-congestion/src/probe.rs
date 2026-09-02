//! Probing: the half of GCC that recovers.
//!
//! Without it the controller climbs by multiplicative increase alone, and 1.08
//! per second means a factor of five takes `ln 5 / ln 1.08`, which is 20.9
//! seconds. That is exactly the recovery time an earlier simulation observed
//! after an 80% capacity cut, and it is not a tuning problem: it is what the
//! algorithm does when nothing tests the link.
//!
//! A probe is a cluster of datagrams released at a chosen rate, higher than the
//! current target, whose arrival spacing in the next report says whether the
//! link carried them. The result sets the estimate directly instead of waiting
//! for the increase to walk there.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::Duration;

use crate::as_rate;
use crate::delay::Signal;
use crate::history::{Acked, ClusterId, micros_of};
use crate::rate::bits_per_second;

/// The first probe of a session, as a multiple of the start rate.
const FIRST_SCALE: f64 = 3.0;

/// Every probe after that, as a multiple of the estimate.
///
/// The second probe of a session is therefore six times the start rate, because
/// the first one succeeded and left the estimate at three times it. The
/// sequence on a 20 Mbit/s ceiling is 6, 12 and 20.
const FURTHER_SCALE: f64 = 2.0;

/// How close the estimate must come to the last probe's rate before the next
/// probe is worth taking.
const FURTHER_THRESHOLD: f64 = 0.7;

/// The longest the controller sits at a rate without testing whether the link
/// has more to give.
///
/// `spec/congestion.md` gates this probe on the sender having been
/// application-limited for five seconds, on the grounds that a remote desktop
/// is application-limited almost all the time. **This deviates**, and drops the
/// gate. A sender that is producing exactly its target is the case where
/// recovery matters most, and it is the one case the gate excludes: the link
/// recovers, the user is doing something, and the controller sits at the
/// depressed rate for as long as they keep doing it. The cost of probing
/// unconditionally is fixed as a share of the link and does not depend on it:
/// probing at twice the rate for 15 ms every 5 seconds is 0.6%, whether the
/// rate is 300 kbit/s or 20 Mbit/s.
const IDLE_INTERVAL: i64 = micros_of(Duration::from_secs(5));

/// A fall this far below the recent high water mark asks for a probe.
const DROP_THRESHOLD: f64 = 0.66;

/// How long a high water mark is remembered.
const DROP_WINDOW: i64 = micros_of(Duration::from_secs(5));

/// What a probe after a fall aims at, as a share of the high water mark.
const AFTER_DROP_SCALE: f64 = 0.85;

/// How long a cluster's result is waited for before it is abandoned.
const RESULT_TIMEOUT: i64 = micros_of(Duration::from_secs(1));

/// The shortest a cluster may last.
const CLUSTER_SPAN: Duration = Duration::from_millis(15);

/// The fewest datagrams a cluster may hold.
///
/// At 300 kbit/s, 15 ms is one datagram and one datagram measures nothing, so
/// the cluster becomes five datagrams spread over 96 ms and the spacing still
/// reads the link.
const CLUSTER_MIN: usize = 5;

/// The fewest that must arrive before the cluster says anything, which is four
/// fifths of what was released.
const CLUSTER_ACCEPT: usize = 4;

/// A cluster the transport is asked to release.
///
/// How many datagrams it takes is the transport's to work out, because only the
/// transport knows how wide a datagram is on this path. [`cluster_datagrams`]
/// is that arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// What to tag the datagrams with, so the result can be read back.
    pub cluster: ClusterId,
    /// Bits a second to release them at.
    pub rate: NonZeroU32,
}

/// How far a cluster's arrivals may outpace its releases before it is not a
/// cluster any more.
const MAX_VALID_RATIO: f64 = 2.0;

/// Below this share of the send rate, the probe saturated the link.
const SATURATED_BELOW: f64 = 0.9;

/// What a saturated probe's result is scaled by, so that acting on it does not
/// immediately overuse.
///
/// libwebrtc uses 0.95 here. `spec/congestion.md` specifies 0.8, and this
/// follows the document: a remote desktop pays for an overshoot with a key
/// frame, which costs eight ordinary frames, so the conservative figure is the
/// right one for this system.
const SATURATED_UTILISATION: f64 = 0.8;

/// One cluster, as much of it as the reports have described.
#[derive(Debug, Clone, Copy)]
struct Cluster {
    first_send: i64,
    last_send: i64,
    last_send_bytes: u64,
    first_receive: i64,
    first_receive_bytes: u64,
    last_receive: i64,
    bytes: u64,
    packets: usize,
}

impl Cluster {
    fn observe(&mut self, packet: &Acked) {
        let wire = u64::from(packet.wire);
        if packet.sent < self.first_send {
            self.first_send = packet.sent;
        }
        if packet.sent > self.last_send {
            self.last_send = packet.sent;
            self.last_send_bytes = wire;
        }
        if packet.arrived < self.first_receive {
            self.first_receive = packet.arrived;
            self.first_receive_bytes = wire;
        }
        self.last_receive = self.last_receive.max(packet.arrived);
        self.bytes += wire;
        self.packets += 1;
    }

    /// What the cluster says the link carries, or `None` while it says nothing.
    fn result(&self) -> Option<f64> {
        if self.packets < CLUSTER_ACCEPT {
            return None;
        }
        let send_span = self.last_send - self.first_send;
        let receive_span = self.last_receive - self.first_receive;
        if send_span <= 0 || receive_span <= 0 || send_span > RESULT_TIMEOUT {
            return None;
        }
        // The last datagram sent had not left over the send interval, and the
        // first received had already arrived when the receive interval opened,
        // so each direction counts every byte but its own edge.
        let send_rate = bits_per_second(self.bytes.saturating_sub(self.last_send_bytes), send_span);
        let receive_rate = bits_per_second(
            self.bytes.saturating_sub(self.first_receive_bytes),
            receive_span,
        );
        if send_rate <= 0.0 || receive_rate / send_rate > MAX_VALID_RATIO {
            // Not delivered as a cluster, so it measured the receiver's own
            // scheduling rather than the path.
            return None;
        }
        if receive_rate < SATURATED_BELOW * send_rate {
            return Some(SATURATED_UTILISATION * receive_rate);
        }
        Some(send_rate.min(receive_rate))
    }
}

/// Decides when to probe, at what rate, and reads the results back.
#[derive(Debug)]
pub(crate) struct Probes {
    next_cluster: u32,
    open: BTreeMap<u32, Cluster>,
    outstanding: Option<(ClusterId, i64, f64)>,
    last_probe_rate: Option<f64>,
    high_water: Vec<(i64, f64)>,
    last_probe_at: Option<i64>,
    ceiling: f64,
}

impl Probes {
    pub(crate) const fn new(ceiling: f64) -> Self {
        Self {
            next_cluster: 1,
            open: BTreeMap::new(),
            outstanding: None,
            last_probe_rate: None,
            high_water: Vec::new(),
            last_probe_at: None,
            ceiling,
        }
    }

    pub(crate) const fn set_ceiling(&mut self, ceiling: f64) {
        self.ceiling = ceiling;
    }

    /// Forgets every probe and high water mark, after the estimate was collapsed
    /// for a stall. Recovery then probes from the floor upward, rather than
    /// aiming a cluster at the rate the link just failed to carry.
    pub(crate) fn reset(&mut self) {
        self.open.clear();
        self.outstanding = None;
        self.last_probe_rate = None;
        self.high_water.clear();
        self.last_probe_at = None;
    }

    /// Files one report's packets against the clusters they belonged to, and
    /// returns the rate any completed cluster measured.
    pub(crate) fn read(&mut self, packets: &[Acked], now: i64) -> Option<f64> {
        for packet in packets {
            let Some(cluster) = packet.cluster else {
                continue;
            };
            let wire = u64::from(packet.wire);
            self.open
                .entry(cluster.0)
                .or_insert(Cluster {
                    first_send: packet.sent,
                    last_send: packet.sent,
                    last_send_bytes: wire,
                    first_receive: packet.arrived,
                    first_receive_bytes: wire,
                    last_receive: packet.arrived,
                    bytes: 0,
                    packets: 0,
                })
                .observe(packet);
        }

        let done: Vec<u32> = self
            .open
            .iter()
            .filter(|(_, cluster)| cluster.result().is_some())
            .map(|(&id, _)| id)
            .collect();
        let mut best = None;
        for id in done {
            let cluster = self.open.remove(&id).expect("listed a moment ago");
            if let Some(rate) = cluster.result() {
                best = Some(best.map_or(rate, |held: f64| held.max(rate)));
                if self.outstanding.is_some_and(|(open, _, _)| open.0 == id) {
                    self.settle();
                }
            }
        }
        self.expire(now);
        best
    }

    /// Gives up on a cluster nothing came back about.
    ///
    /// The rate it was released at is deliberately not remembered: a probe
    /// whose result was lost says nothing about the link, and judging the climb
    /// against it would stop the session probing on the strength of a
    /// measurement that never happened.
    fn expire(&mut self, now: i64) {
        self.open
            .retain(|_, cluster| now - cluster.last_receive <= RESULT_TIMEOUT);
        if let Some((_, sent, _)) = self.outstanding
            && now - sent > RESULT_TIMEOUT
        {
            self.outstanding = None;
        }
    }

    /// Takes the result of the cluster that was outstanding.
    fn settle(&mut self) {
        if let Some((_, _, rate)) = self.outstanding.take() {
            self.last_probe_rate = Some(rate);
        }
    }

    /// Whether a probe is due now, and what it should look like.
    pub(crate) fn due(&mut self, estimate: f64, signal: Signal, now: i64) -> Option<Probe> {
        self.remember(estimate, now);
        // One cluster at a time, and no faster than a result comes back. That
        // second bound is what keeps a link whose reports go missing from
        // probing continuously.
        //
        // Never while a queue is filling: a probe is deliberately above the
        // rate the path is refusing, and adding to a queue that is already
        // growing measures nothing and costs a frame.
        if self.outstanding.is_some()
            || signal == Signal::Overuse
            || self
                .last_probe_at
                .is_some_and(|at| now - at < RESULT_TIMEOUT)
        {
            return None;
        }

        let rate = self.wanted(estimate, now)?;
        let rate = rate.min(self.ceiling);
        if rate <= estimate {
            return None;
        }
        let cluster = ClusterId(self.next_cluster);
        self.next_cluster = self.next_cluster.wrapping_add(1).max(1);
        self.outstanding = Some((cluster, now, rate));
        self.last_probe_at = Some(now);
        Some(Probe {
            cluster,
            rate: as_rate(rate),
        })
    }

    /// The rate the next probe should aim at, if one is due at all.
    fn wanted(&self, estimate: f64, now: i64) -> Option<f64> {
        let Some(last) = self.last_probe_rate else {
            // The first key frame of the session has to be sent anyway, so
            // releasing it at three times the start rate measures the link for
            // free.
            return Some(FIRST_SCALE * estimate);
        };
        // A probe the estimate kept up with earns the next one at once, which
        // is what takes a session from 2 to 20 Mbit/s in about three probes
        // rather than the 20.9 seconds multiplicative increase would take.
        if estimate >= FURTHER_THRESHOLD * last && last < self.ceiling {
            return Some(FURTHER_SCALE * estimate);
        }
        self.steady(estimate, now)
    }

    /// The two reasons to probe once the climb has stalled: an estimate that
    /// fell away from a recent high water mark, and simply not having tested
    /// the link for five seconds.
    fn steady(&self, estimate: f64, now: i64) -> Option<f64> {
        if self
            .last_probe_at
            .is_some_and(|at| now - at < IDLE_INTERVAL)
        {
            return None;
        }
        let high = self
            .high_water
            .iter()
            .map(|(_, rate)| *rate)
            .fold(0.0f64, f64::max);
        if high > 0.0 && estimate < DROP_THRESHOLD * high {
            return Some(AFTER_DROP_SCALE * high);
        }
        (estimate < self.ceiling).then_some(FURTHER_SCALE * estimate)
    }

    fn remember(&mut self, estimate: f64, now: i64) {
        self.high_water.push((now, estimate));
        self.high_water.retain(|(at, _)| now - at <= DROP_WINDOW);
    }
}

/// The datagrams a cluster needs to be readable at `rate`.
#[must_use]
pub fn cluster_datagrams(rate: NonZeroU32, datagram_bytes: usize) -> usize {
    let span = u64::from(rate.get()) * micros_of(CLUSTER_SPAN).unsigned_abs() / 8_000_000;
    let bytes = datagram_bytes.max(1);
    usize::try_from(span)
        .unwrap_or(usize::MAX)
        .div_ceil(bytes)
        .max(CLUSTER_MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(cluster: u32, index: i64, send_gap: i64, arrive_gap: i64) -> Acked {
        Acked {
            sent: index * send_gap,
            arrived: 1_000_000 + index * arrive_gap,
            wire: 1434,
            cluster: Some(ClusterId(cluster)),
        }
    }

    #[test]
    fn a_session_opens_by_probing_at_three_times_the_start_rate() {
        let mut probes = Probes::new(20_000_000.0);
        let first = probes
            .due(2_000_000.0, Signal::Normal, 0)
            .expect("a session with no estimate probes at once");
        assert_eq!(first.rate.get(), 6_000_000);
        assert!(
            probes.due(2_000_000.0, Signal::Normal, 100_000).is_none(),
            "one probe is outstanding at a time"
        );
    }

    #[test]
    fn a_cluster_the_link_carried_reports_what_it_carried() {
        let mut probes = Probes::new(20_000_000.0);
        probes.due(2_000_000.0, Signal::Normal, 0);
        // Eight datagrams released 1.5 ms apart, which is 7.6 Mbit/s, arriving
        // at the same spacing: the link took them.
        let packets: Vec<Acked> = (0..8).map(|i| packet(1, i, 1_500, 1_500)).collect();
        let rate = probes.read(&packets, 2_000_000).expect("a full cluster");
        assert!(
            (7_000_000.0..8_200_000.0).contains(&rate),
            "1434 bytes every 1.5 ms is 7.6 Mbit/s, and this said {rate}"
        );
    }

    #[test]
    fn a_cluster_the_link_could_not_carry_reports_less_than_it_was_sent_at() {
        let mut probes = Probes::new(20_000_000.0);
        probes.due(2_000_000.0, Signal::Normal, 0);
        // Released at 11.5 Mbit/s and arriving at 5.7: the probe saturated the
        // link.
        let packets: Vec<Acked> = (0..8).map(|i| packet(1, i, 1_000, 2_000)).collect();
        let rate = probes.read(&packets, 2_000_000).expect("a full cluster");
        assert!(
            rate < 5_000_000.0,
            "a saturated probe must be discounted, and this said {rate}"
        );
    }

    #[test]
    fn a_cluster_that_did_not_arrive_as_one_is_discarded() {
        let mut probes = Probes::new(20_000_000.0);
        probes.due(2_000_000.0, Signal::Normal, 0);
        // Arriving four times faster than it was sent: whatever this measured,
        // it was not the path.
        let packets: Vec<Acked> = (0..8).map(|i| packet(1, i, 4_000, 1_000)).collect();
        assert_eq!(probes.read(&packets, 2_000_000), None);
    }

    #[test]
    fn a_probe_nothing_came_back_about_is_abandoned_rather_than_waited_on() {
        let mut probes = Probes::new(20_000_000.0);
        probes.due(2_000_000.0, Signal::Normal, 0);
        probes.read(&[], RESULT_TIMEOUT + 1);
        assert!(
            probes
                .due(2_000_000.0, Signal::Normal, RESULT_TIMEOUT + 2)
                .is_some(),
            "a lost cluster must not stop the session probing for ever"
        );
    }

    #[test]
    fn a_link_that_nothing_has_tested_for_five_seconds_is_tested() {
        // The specific answer to a controller that would otherwise sit at a
        // depressed rate for ever after one transient.
        let mut probes = Probes::new(20_000_000.0);
        probes.last_probe_rate = Some(12_000_000.0);
        probes.last_probe_at = Some(0);
        let mut now = 0;
        for _ in 0..80 {
            now += micros_of(Duration::from_millis(50));
            assert!(probes.due(4_000_000.0, Signal::Normal, now).is_none());
        }
        let mut probe = None;
        while now < IDLE_INTERVAL + micros_of(Duration::from_secs(1)) {
            now += micros_of(Duration::from_millis(50));
            probe = probe.or_else(|| probes.due(4_000_000.0, Signal::Normal, now));
        }
        let probe = probe.expect("a link nothing has tested must be tested");
        assert_eq!(probe.rate.get(), 8_000_000, "twice the current estimate");
    }

    #[test]
    fn nothing_probes_into_a_queue_that_is_already_filling() {
        let mut probes = Probes::new(20_000_000.0);
        assert!(
            probes.due(2_000_000.0, Signal::Overuse, 0).is_none(),
            "a probe is deliberately above the rate the path is refusing"
        );
        assert!(probes.due(2_000_000.0, Signal::Normal, 1).is_some());
    }

    #[test]
    fn a_fall_away_from_the_recent_high_water_mark_is_tested_at_the_mark() {
        let mut probes = Probes::new(20_000_000.0);
        // A probe the estimate never caught up with, so the climb has stalled
        // and the steady rules are what decide.
        probes.last_probe_rate = Some(14_500_000.0);
        probes.last_probe_at = Some(-IDLE_INTERVAL);
        probes.remember(10_000_000.0, 0);

        let probe = probes
            .due(1_500_000.0, Signal::Normal, 1)
            .expect("a fall to fifteen percent of the mark must be tested");
        assert_eq!(
            probe.rate.get(),
            8_500_000,
            "eighty-five percent of the high water mark"
        );
    }

    #[test]
    fn a_cluster_is_at_least_five_datagrams_however_low_the_rate() {
        let low = NonZeroU32::new(300_000).unwrap();
        assert_eq!(cluster_datagrams(low, 1434), CLUSTER_MIN);
        let high = NonZeroU32::new(40_000_000).unwrap();
        assert_eq!(cluster_datagrams(high, 1434), 53);
    }
}
