//! What the host put on the path, and how a receiver report is matched to it.
//!
//! The report names datagrams by the `seq` already in the media header, which
//! is sixteen bits and wraps in under 40 seconds at 20 Mbit/s. Numbers are
//! extended to 64 bits on the way in, so the wrap is invisible for as long as
//! reports keep arriving, and a report whose base cannot be placed against what
//! was sent discards the record rather than being trusted.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tachyx_proto::control::{FEEDBACK_TICK, Feedback, PacketArrival};
use tachyx_proto::media::Seq;

/// How long a datagram's release is remembered.
///
/// Reports arrive every 50 ms, so three seconds is sixty of them. At 20 Mbit/s
/// that is about 5,300 records, or 127 kB.
const HORIZON: Duration = Duration::from_secs(3);

/// Which probe a datagram was released as part of.
///
/// A cluster is read as a whole: its first and last release, its first and last
/// arrival, and the bytes between, are what say whether the link carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClusterId(pub u32);

/// One datagram, as the host released it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    /// Its position in the session-wide datagram sequence, which is the key the
    /// receiver report is written against.
    pub seq: Seq,
    /// When it was handed to the transport.
    pub at: Instant,
    /// Bytes it weighed on the wire, header included.
    pub wire: u16,
    /// The probe cluster it belonged to, if it was released as part of one.
    pub cluster: Option<ClusterId>,
}

/// One datagram the client said arrived, matched to the record of its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Acked {
    /// Host clock at release, in microseconds since the controller started.
    pub(crate) sent: i64,
    /// Client clock at arrival, in microseconds since the client's session
    /// began. Only ever differenced against another of these, so the offset
    /// between the two clocks cancels.
    pub(crate) arrived: i64,
    /// Bytes on the wire.
    pub(crate) wire: u16,
    /// The probe cluster it belonged to.
    pub(crate) cluster: Option<ClusterId>,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    extended: u64,
    sent: i64,
    wire: u16,
    cluster: Option<ClusterId>,
}

/// The datagrams released recently enough for a report to still be about them.
#[derive(Debug)]
pub(crate) struct History {
    epoch: Instant,
    records: VecDeque<Record>,
    newest: Option<u64>,
}

impl History {
    pub(crate) const fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            records: VecDeque::new(),
            newest: None,
        }
    }

    /// Records one release, extending its sequence number and forgetting
    /// anything older than [`HORIZON`].
    pub(crate) fn push(&mut self, release: &Release) {
        let extended = match self.newest {
            None => u64::from(release.seq.0),
            Some(newest) => {
                let ahead = release.seq.distance_from(Seq::wire(newest));
                match newest.checked_add_signed(i64::from(ahead)) {
                    Some(extended) if extended > newest => extended,
                    // Out of order or repeated. The packetiser stamps in order,
                    // so this is a caller fault rather than a path event, and
                    // keeping the record monotone is what lets a report be
                    // matched by binary search.
                    _ => return,
                }
            }
        };
        let sent = self.micros(release.at);
        self.newest = Some(extended);
        self.records.push_back(Record {
            extended,
            sent,
            wire: release.wire,
            cluster: release.cluster,
        });
        let oldest = sent.saturating_sub(micros_of(HORIZON));
        while self
            .records
            .front()
            .is_some_and(|first| first.sent < oldest)
        {
            self.records.pop_front();
        }
    }

    /// Forgets everything, after a report that could not be placed.
    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.newest = None;
    }

    /// Host clock in microseconds since this controller started.
    pub(crate) fn micros(&self, at: Instant) -> i64 {
        i64::try_from(at.saturating_duration_since(self.epoch).as_micros()).unwrap_or(i64::MAX)
    }

    /// The datagrams `report` says arrived, in send order, dropping any whose
    /// record has been forgotten.
    ///
    /// `None` when the report's base cannot be placed against what was sent at
    /// all, which is the caller's signal to discard the history and restart.
    pub(crate) fn read(&self, report: &Feedback) -> Option<Vec<Acked>> {
        let newest = self.newest?;
        let behind = Seq::wire(newest).distance_from(report.base);
        // A report can only ever name datagrams the host has already released,
        // so its base is behind the newest one. Anything else means the two
        // ends disagree about where the sequence is.
        if behind < 0 {
            return None;
        }
        let base = newest.checked_sub(behind.unsigned_abs().into())?;

        let mut acked = Vec::with_capacity(report.packets.len());
        let mut arrival: Option<i64> = None;
        for (offset, status) in report.packets.iter().enumerate() {
            let PacketArrival::Received(delta) = status else {
                continue;
            };
            // The first arrival is measured from the report's reference and the
            // rest from the one before, so a delta is a byte where an offset
            // would be two.
            let at = arrival.unwrap_or(i64::from(report.reference) * micros_of(FEEDBACK_TICK))
                + i64::from(*delta) * micros_of(FEEDBACK_TICK);
            arrival = Some(at);
            let Some(record) = self.get(base.saturating_add(offset as u64)) else {
                continue;
            };
            acked.push(Acked {
                sent: record.sent,
                arrived: at,
                wire: record.wire,
                cluster: record.cluster,
            });
        }
        Some(acked)
    }

    fn get(&self, extended: u64) -> Option<Record> {
        let index = self
            .records
            .binary_search_by_key(&extended, |record| record.extended)
            .ok()?;
        self.records.get(index).copied()
    }
}

/// A duration in microseconds, for arithmetic that has to go negative.
#[expect(
    clippy::cast_possible_truncation,
    reason = "every duration here is a constant of a few seconds at most"
)]
pub(crate) const fn micros_of(duration: Duration) -> i64 {
    duration.as_micros() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn released(history: &mut History, from: u16, count: u16, at: Instant) {
        for offset in 0..count {
            history.push(&Release {
                seq: Seq(from.wrapping_add(offset)),
                at: at + Duration::from_micros(u64::from(offset) * 100),
                wire: 1434,
                cluster: None,
            });
        }
    }

    fn report(base: u16, count: usize) -> Feedback {
        Feedback {
            base: Seq(base),
            reference: 4_000,
            // The first arrival is measured from the reference, so its delta
            // is zero; every one after it is measured from the one before.
            packets: (0..count)
                .map(|index| PacketArrival::Received(if index == 0 { 0 } else { 4 }))
                .collect(),
            lost: 0,
            received: count as u64,
        }
    }

    #[test]
    fn a_report_is_matched_across_the_sequence_wrap() {
        let start = Instant::now();
        let mut history = History::new(start);
        // Straddling the wrap: 65530 to 5 inclusive.
        released(&mut history, 65_530, 12, start);

        let acked = history
            .read(&report(65_530, 12))
            .expect("the base was released a moment ago");
        assert_eq!(acked.len(), 12);
        assert!(
            acked.windows(2).all(|pair| pair[0].sent < pair[1].sent),
            "the wrap must not reorder the record"
        );
        assert_eq!(acked[0].arrived, 1_000_000, "reference is 4000 ticks");
        assert_eq!(acked[1].arrived, 1_001_000, "one tick is 250 microseconds");
    }

    #[test]
    fn a_datagram_whose_record_has_expired_is_skipped_rather_than_guessed() {
        let start = Instant::now();
        let mut history = History::new(start);
        released(&mut history, 0, 4, start);
        released(&mut history, 4, 4, start + HORIZON + Duration::from_secs(1));

        let acked = history.read(&report(0, 8)).expect("placeable");
        assert_eq!(acked.len(), 4, "the four inside the horizon");
    }

    #[test]
    fn a_report_naming_datagrams_the_host_never_sent_is_refused() {
        let start = Instant::now();
        let mut history = History::new(start);
        released(&mut history, 0, 10, start);
        assert!(
            history.read(&report(500, 4)).is_none(),
            "a base ahead of everything released cannot be placed"
        );
        assert!(History::new(start).read(&report(0, 4)).is_none());
    }

    #[test]
    fn missing_statuses_carry_the_arrival_clock_forward() {
        let start = Instant::now();
        let mut history = History::new(start);
        released(&mut history, 0, 3, start);
        let acked = history
            .read(&Feedback {
                base: Seq(0),
                reference: 0,
                packets: vec![
                    PacketArrival::Received(0),
                    PacketArrival::Missing,
                    PacketArrival::Received(8),
                ],
                lost: 1,
                received: 2,
            })
            .expect("placeable");
        assert_eq!(acked.len(), 2, "a missing datagram has no arrival");
        assert_eq!(acked[1].arrived, 2_000, "eight ticks after the first");
    }
}
