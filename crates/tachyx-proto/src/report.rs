//! Telling the host when each datagram arrived.
//!
//! The other half of [`crate::packet::Reassembler`]'s sequence accounting: that
//! one counts what was lost and delivers pictures, this one says when each
//! datagram landed. Both are keyed on the same `seq` and both run on the
//! client.
//!
//! This is the only measurement of the path the host can have: quinn exposes no
//! per-datagram acknowledgement to the application, and its own bandwidth
//! estimate was measured reporting zero on a healthy hundred-megabit link. So
//! the report is not an optimisation, and it runs on a phone, which is why the
//! arrivals live in a ring indexed by sequence number rather than in a map.
//!
//! No clock synchronisation is required and none is implied. Every quantity the
//! host takes from a report is a difference between two of these arrival times,
//! so a constant offset between the two clocks cancels exactly.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::control::{FEEDBACK_MAX_STATUSES, FEEDBACK_TICK, Feedback, PacketArrival};
use crate::media::Seq;

/// The most arrivals held before the oldest are given up on.
///
/// Four reports' worth. Anything older than that has already been reported or
/// has been overtaken by a report the host will read as loss, and holding it
/// would only grow a buffer a client cannot drain.
const WINDOW: usize = 4 * FEEDBACK_MAX_STATUSES;

/// When each datagram arrived, and the report written from it.
#[derive(Debug)]
pub struct Reporter {
    started: Instant,
    /// Arrival ticks, indexed from `base`. `None` is a sequence number that has
    /// not been seen.
    window: VecDeque<Option<u32>>,
    /// The extended sequence number `window[0]` describes.
    base: u64,
    /// The highest extended sequence number seen, for placing the next one.
    highest: Option<u64>,
    due: Option<Instant>,
    /// Datagrams reported received, cumulative over the session.
    received: u64,
    /// Datagrams reported missing, cumulative over the session.
    ///
    /// Counted when a slot leaves the window still empty, not when the gap
    /// first shows. The difference is every packet a wireless link merely
    /// reorders: the reassembler counts a gap as loss the instant a later
    /// sequence number arrives, so on the air hop this was measured against,
    /// where 22.92% of packets arrive more than 2 ms behind the fastest, its
    /// counter reads milliseconds of jitter as loss the host then throttles
    /// the whole session for. A slot here has until the report drains it, which
    /// is a report interval, to be filled late and counted as the arrival it
    /// was.
    lost: u64,
}

impl Reporter {
    /// Starts a reporter whose clock runs from `now`.
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self {
            started: now,
            window: VecDeque::new(),
            base: 0,
            highest: None,
            due: None,
            received: 0,
            lost: 0,
        }
    }

    /// Records one video datagram's arrival.
    ///
    /// Padding and parity are recorded like any other: they cost the path the
    /// same bytes and their loss is the same evidence about it.
    pub fn record(&mut self, seq: Seq, now: Instant) {
        let Some(extended) = self.place(seq) else {
            return;
        };
        self.highest = Some(self.highest.map_or(extended, |held| held.max(extended)));
        let Ok(index) = usize::try_from(extended - self.base) else {
            return;
        };
        while self.window.len() <= index {
            self.window.push_back(None);
        }
        // The first arrival wins. A duplicate says nothing new about the path
        // and would overwrite the measurement with a later one.
        let tick = self.ticks(now);
        if let Some(slot @ None) = self.window.get_mut(index) {
            *slot = Some(tick);
        }
        // A client that cannot send reports must not grow a buffer, and the
        // slots it gives up on are still part of the cumulative count.
        while self.window.len() > WINDOW {
            let slot = self.window.pop_front().flatten();
            self.settle(slot);
            self.base += 1;
        }
    }

    /// Counts one slot leaving the window, filled or not.
    const fn settle(&mut self, slot: Option<u32>) {
        if slot.is_some() {
            self.received += 1;
        } else {
            self.lost += 1;
        }
    }

    /// The report to send now, if one is due.
    ///
    /// The cumulative `lost` and `received` counters the report carries are
    /// this reporter's own, counted as slots leave the window, because they
    /// feed the host's loss controller and that controller throttles the
    /// session. The reassembler's counters are the wrong source for them: they
    /// call a packet lost the instant a later one arrives and never take it
    /// back, so on a wireless link they read reordering as loss. Measured on
    /// the phone this was built against: 211 counted lost while 429 arrived
    /// late, the loss controller held the session at its 2 Mbit/s opening
    /// target for the whole run, and the parity level was sized for loss that
    /// was not happening.
    ///
    /// A caller drains this in a loop: a report carries at most
    /// [`FEEDBACK_MAX_STATUSES`] statuses and a longer one is split into parts
    /// sent back to back.
    pub fn poll(&mut self, now: Instant, interval: Duration) -> Option<Feedback> {
        if self.due.is_some_and(|due| now < due) {
            return None;
        }
        // Trailing gaps are not reported: a datagram that has not arrived yet
        // may still arrive, and saying it is missing costs a report and might
        // be wrong.
        let described = self
            .window
            .iter()
            .rposition(Option::is_some)
            .map(|index| index + 1);
        let Some(take) = described.map(|count| count.min(FEEDBACK_MAX_STATUSES)) else {
            // Nothing arrived since the last report, so there is nothing to
            // say.
            self.due = Some(now + interval);
            return None;
        };

        let mut packets = Vec::with_capacity(take);
        let mut reference = None;
        let mut previous = None;
        let mut lost = self.lost;
        let mut received = self.received;
        for slot in self.window.drain(..take) {
            let Some(tick) = slot else {
                lost += 1;
                packets.push(PacketArrival::Missing);
                continue;
            };
            received += 1;
            let delta = previous.map_or_else(
                || {
                    reference = Some(tick);
                    0
                },
                |held| i64::from(tick) - i64::from(held),
            );
            previous = Some(tick);
            packets.push(PacketArrival::Received(
                i16::try_from(delta).unwrap_or(if delta < 0 { i16::MIN } else { i16::MAX }),
            ));
        }

        let base = Seq::wire(self.base);
        self.base += take as u64;
        self.lost = lost;
        self.received = received;
        // A report that filled to the cap leaves a backlog, and the parts go
        // out back to back rather than one every interval.
        self.due = Some(if self.window.iter().any(Option::is_some) {
            now
        } else {
            now + interval
        });
        Some(Feedback {
            base,
            reference: reference.unwrap_or_else(|| self.ticks(now)),
            packets,
            lost,
            received,
        })
    }

    /// When the next report is due, if anything is waiting to go in one.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.window
            .iter()
            .any(Option::is_some)
            .then_some(self.due)?
    }

    /// The extended number for `seq`, or `None` when it names something already
    /// reported.
    fn place(&mut self, seq: Seq) -> Option<u64> {
        let Some(highest) = self.highest else {
            self.base = u64::from(seq.0);
            return Some(self.base);
        };
        let extended =
            highest.checked_add_signed(i64::from(seq.distance_from(Seq::wire(highest))))?;
        (extended >= self.base).then_some(extended)
    }

    /// The client's own clock, in the units the report is written in.
    fn ticks(&self, now: Instant) -> u32 {
        let elapsed = now.saturating_duration_since(self.started);
        u32::try_from(elapsed.as_micros() / FEEDBACK_TICK.as_micros()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = FEEDBACK_TICK;
    const INTERVAL: Duration = Duration::from_millis(50);

    fn arrive(reporter: &mut Reporter, start: Instant, from: u16, count: u16, spacing: Duration) {
        for offset in 0..count {
            reporter.record(
                Seq(from.wrapping_add(offset)),
                start + spacing * u32::from(offset),
            );
        }
    }

    #[test]
    fn a_report_describes_every_sequence_number_from_its_base_upward() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        arrive(&mut reporter, start, 100, 6, TICK * 4);

        let report = reporter
            .poll(start + INTERVAL, INTERVAL)
            .expect("six datagrams arrived");
        assert_eq!(report.base, Seq(100));
        assert_eq!(report.packets.len(), 6);
        assert_eq!(report.reference, 0);
        assert_eq!(report.packets[0], PacketArrival::Received(0));
        assert_eq!(report.packets[1], PacketArrival::Received(4));
        assert_eq!(report.received, 6);
        assert!(
            reporter.poll(start + INTERVAL, INTERVAL).is_none(),
            "nothing arrived since, so there is nothing to say"
        );
    }

    #[test]
    fn a_gap_is_reported_as_missing_and_the_clock_carries_on() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        reporter.record(Seq(0), start);
        reporter.record(Seq(2), start + TICK * 9);

        let report = reporter
            .poll(start + INTERVAL, INTERVAL)
            .expect("two datagrams arrived");
        assert_eq!(
            report.packets,
            vec![
                PacketArrival::Received(0),
                PacketArrival::Missing,
                PacketArrival::Received(9)
            ]
        );
        assert_eq!(report.lost, 1);
    }

    #[test]
    fn reordering_is_reported_rather_than_hidden() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        reporter.record(Seq(0), start);
        reporter.record(Seq(2), start + TICK * 4);
        // The one in between arrives late, which a link-layer retry does.
        reporter.record(Seq(1), start + TICK * 20);

        let report = reporter
            .poll(start + INTERVAL, INTERVAL)
            .expect("three datagrams arrived");
        assert_eq!(
            report.packets,
            vec![
                PacketArrival::Received(0),
                PacketArrival::Received(20),
                PacketArrival::Received(-16)
            ],
            "an unsigned delta would have to lie about this"
        );
    }

    /// A link-layer retry delivers a packet milliseconds late, which is not a
    /// loss and must not be reported as one: the host's loss controller
    /// throttles the session on these counters. The slot has until a report
    /// drains it to be filled.
    #[test]
    fn a_packet_that_arrives_late_is_an_arrival_and_not_a_loss() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        reporter.record(Seq(0), start);
        reporter.record(Seq(2), start + TICK * 4);
        reporter.record(Seq(1), start + TICK * 30);

        let report = reporter
            .poll(start + INTERVAL, INTERVAL)
            .expect("three datagrams arrived");
        assert_eq!(report.lost, 0, "a reordered packet is not a lost one");
        assert_eq!(report.received, 3);
    }

    #[test]
    fn a_report_is_split_rather_than_truncated() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        let described = u16::try_from(FEEDBACK_MAX_STATUSES).unwrap() + 200;
        arrive(&mut reporter, start, 0, described, TICK);

        let now = start + INTERVAL;
        let first = reporter.poll(now, INTERVAL).expect("a full report");
        assert_eq!(first.packets.len(), FEEDBACK_MAX_STATUSES);
        assert_eq!(first.base, Seq(0));
        let second = reporter
            .poll(now, INTERVAL)
            .expect("the parts go out back to back");
        assert_eq!(second.packets.len(), 200);
        assert_eq!(
            second.base,
            Seq(u16::try_from(FEEDBACK_MAX_STATUSES).unwrap())
        );
        assert!(reporter.poll(now, INTERVAL).is_none());
    }

    #[test]
    fn the_cadence_holds_while_datagrams_keep_arriving() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        let mut reports = 0;
        for slot in 0..1_000u32 {
            let now = start + Duration::from_micros(u64::from(slot) * 600);
            reporter.record(Seq(u16::try_from(slot).unwrap()), now);
            if reporter.poll(now, INTERVAL).is_some() {
                reports += 1;
            }
        }
        // Six hundred milliseconds at fifty is twelve, plus the first.
        assert!(
            (11..=13).contains(&reports),
            "{reports} reports in six hundred milliseconds"
        );
    }

    #[test]
    fn the_sequence_wrap_does_not_reorder_the_window() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        arrive(&mut reporter, start, 65_530, 12, TICK);
        let report = reporter
            .poll(start + INTERVAL, INTERVAL)
            .expect("twelve datagrams arrived");
        assert_eq!(report.base, Seq(65_530));
        assert_eq!(report.packets.len(), 12);
        assert!(
            report
                .packets
                .iter()
                .all(|status| matches!(status, PacketArrival::Received(_))),
            "the wrap must not open a hole"
        );
    }

    #[test]
    fn nothing_is_held_for_ever() {
        let start = Instant::now();
        let mut reporter = Reporter::new(start);
        // A client that never sends a report still must not grow a buffer.
        for slot in 0..40_000u32 {
            reporter.record(
                Seq(u16::try_from(slot % 65_536).unwrap()),
                start + Duration::from_micros(u64::from(slot) * 600),
            );
        }
        assert!(reporter.window.len() <= WINDOW);
    }
}
