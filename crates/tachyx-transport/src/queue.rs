//! The one record of quinn's outgoing datagram queue.
//!
//! quinn reports how many payload bytes that queue holds and never how many
//! datagrams hold them, while its eviction charges accounting per datagram. So
//! the count has to be reconstructed, and the only way to reconstruct it is to
//! keep a record of what was pushed and match its tail against the bytes quinn
//! reports. That is exact while every datagram on the queue is in the record,
//! and worth nothing the moment one is not.
//!
//! Sound is what made that a real problem rather than a note. Two threads write
//! to one queue, so there is one record, and a datagram has to enter the queue
//! and the record together or the two disagree for as long as the race lasts. A
//! record that disagrees is a record that cannot be squared, which shuts the
//! video gate: the frame is refused **after** it was encoded, which spends a
//! frame identifier and costs a key frame. Holding the lock across the
//! hand-over is what makes the order of the record the order of the queue.
//!
//! The room check lives here too, on the same lock, for the same reason. The
//! gate in [`crate::video`] asks for room for a whole frame before the frame is
//! encoded, which is a decision about what to produce; this is the interlock
//! that decides whether one datagram may be handed over at all. Both are
//! needed: the first keeps the encoder's reference chain intact, and only the
//! second can promise that nothing ever reaches quinn's eviction path, because
//! between the gate and the last datagram of a paced key frame there are tens
//! of milliseconds in which sound is handed over too.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use quinn::{Connection, SendDatagramError};

/// Bytes of accounting quinn charges for each queued datagram, on top of its
/// payload.
///
/// Its outgoing queue holds `quinn_proto::frame::Datagram`, which is one
/// `Bytes` and nothing else, and charges `size_of` that per entry. This is the
/// term [`quinn::Connection::datagram_send_buffer_space`] leaves out: that
/// report subtracts one of these from the whole buffer, while the eviction loop
/// it is supposed to predict charges one per datagram already queued. At the
/// ninety-two datagrams a 1080p key frame occupies, the difference is 2944
/// bytes the report does not show.
pub(crate) const QUEUE_OVERHEAD: usize = size_of::<Bytes>();

/// What became of one datagram offered to the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handed {
    /// The connection took it, and the record holds it.
    Queued,
    /// The queue had no room for it, so it was never offered.
    ///
    /// Refusing costs this datagram. Offering it would cost the connection:
    /// quinn discards the oldest queued datagrams to make room, and the discard
    /// path in quinn-proto 0.11.17 subtracts a dropped datagram's length from
    /// its own accounting twice. See [`crate::video`].
    NoRoom,
}

/// quinn's outgoing datagram queue, as everything that writes to it sees it.
///
/// Shared by the picture path and the sound path, because there is one queue
/// and a record of it that only one of them writes to is a record of half of
/// it.
#[derive(Debug)]
pub(crate) struct Outgoing {
    connection: Connection,
    /// The size quinn was configured to give the outgoing datagram queue.
    /// Nothing on the connection reports it back, and the record cannot be
    /// squared without it.
    buffer: usize,
    /// Sizes of the datagrams pushed onto that queue, oldest first, less the
    /// ones quinn is known to have taken.
    record: Mutex<VecDeque<usize>>,
}

impl Outgoing {
    /// Records the queue of `connection`, which holds `buffer` bytes.
    pub(crate) const fn new(connection: Connection, buffer: usize) -> Self {
        Self {
            connection,
            buffer,
            record: Mutex::new(VecDeque::new()),
        }
    }

    /// The connection itself, for everything that is not a datagram.
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Hands over one datagram of `wire` bytes, built only once there is room
    /// for it.
    ///
    /// The room check and the hand-over are one operation. Split in two they
    /// would be a check against a queue another thread is free to fill before
    /// the datagram reaches it, and the whole point of the check is that
    /// nothing ever reaches the eviction path.
    ///
    /// `build` runs under the same lock, and late, because a datagram is
    /// stamped with a sequence number as it is built. A number spent on a
    /// datagram that never leaves is a hole in the sequence, which the receiver
    /// reports as a lost packet and the congestion controller then reads as the
    /// path losing it.
    ///
    /// # Errors
    /// If the connection has ended, or the peer will not take datagrams.
    pub(crate) fn send(
        &self,
        wire: usize,
        build: impl FnOnce() -> Vec<u8>,
    ) -> Result<Handed, SendDatagramError> {
        let mut record = self.record();
        let space = self.connection.datagram_send_buffer_space();
        if !room_for(&mut record, self.held_at(space), space, 1, wire) {
            return Ok(Handed::NoRoom);
        }
        let datagram = build();
        let queued = datagram.len();
        self.connection.send_datagram(Bytes::from(datagram))?;
        record.push_back(queued);
        Ok(Handed::Queued)
    }

    /// Whether the queue has room for `datagrams` of `wire` bytes each without
    /// discarding anything already on it.
    ///
    /// `wire` is the whole datagram, header included, because that is what
    /// quinn queues and charges for.
    pub(crate) fn admits(&self, datagrams: usize, wire: usize) -> bool {
        let mut record = self.record();
        let space = self.connection.datagram_send_buffer_space();
        room_for(&mut record, self.held_at(space), space, datagrams, wire)
    }

    /// Forgets the datagrams quinn has taken off the queue.
    ///
    /// Only ever an economy: the record squares itself on every send and every
    /// question asked of it. This is for the long pause in the middle of a
    /// paced key frame, where neither happens for milliseconds at a time.
    pub(crate) fn prune(&self) {
        let mut record = self.record();
        let space = self.connection.datagram_send_buffer_space();
        square(&mut record, self.held_at(space));
    }

    /// Payload bytes quinn's outgoing queue holds.
    pub(crate) fn held(&self) -> usize {
        self.held_at(self.connection.datagram_send_buffer_space())
    }

    /// The same, from a reading of the free space already taken.
    ///
    /// The report is the buffer less those bytes less one entry's accounting,
    /// so adding that entry back leaves the bytes themselves.
    const fn held_at(&self, space: usize) -> usize {
        self.buffer.saturating_sub(space + QUEUE_OVERHEAD)
    }

    /// A poisoned lock means a send panicked part way through. What is behind
    /// it is still the record of a queue, and reading it beats killing the
    /// session over it: a record that cannot be squared refuses sends until the
    /// queue drains, which is exactly what this already does for a queue quinn
    /// has taken a datagram out of the middle of.
    fn record(&self) -> MutexGuard<'_, VecDeque<usize>> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `datagrams` of `wire` bytes each fit, squaring the record first.
///
/// The header and the queue's own accounting are charged per datagram rather
/// than once, for the datagrams being offered and for every one already
/// queued. Both are terms [`quinn::Connection::datagram_send_buffer_space`]
/// omits: it subtracts a single entry's accounting from the buffer, while the
/// eviction loop it is meant to predict charges one per entry. The report is
/// therefore too generous as soon as two datagrams are queued ahead, and
/// believing it reaches an eviction that does not merely lose a frame but ends
/// the connection.
fn room_for(
    record: &mut VecDeque<usize>,
    held: usize,
    space: usize,
    datagrams: usize,
    wire: usize,
) -> bool {
    let Some(queued) = square(record, held) else {
        return false;
    };
    let required = datagrams.saturating_mul(wire + QUEUE_OVERHEAD);
    // `space` is the buffer less the payload bytes queued less one entry's
    // accounting, so adding that entry back gives the bytes the queue has free,
    // from which every entry in it charges its own.
    required <= (space + QUEUE_OVERHEAD).saturating_sub(queued * QUEUE_OVERHEAD)
}

/// Drops what quinn has taken and reports what it still holds, or `None` when
/// the record cannot be squared with the bytes quinn reports.
///
/// quinn takes from the front, so what is left is the tail of what was pushed,
/// and the tail whose sizes add up to the bytes reported is the only one it can
/// be.
///
/// When they do not add up, something removed a datagram from the middle: a
/// path that shrinks makes quinn drop the queued datagrams too large for it.
/// The count is then unknown, the gate refuses everything, and the record
/// squares itself again the moment the queue drains.
fn square(record: &mut VecDeque<usize>, held: usize) -> Option<usize> {
    let mut bytes = 0;
    let mut entries = 0;
    for size in record.iter().rev() {
        if bytes >= held {
            break;
        }
        bytes += size;
        entries += 1;
    }
    if bytes != held {
        return None;
    }
    record.drain(..record.len() - entries);
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squaring_forgets_what_quinn_has_taken() {
        let mut record: VecDeque<usize> = [100, 200, 300].into_iter().collect();
        assert_eq!(square(&mut record, 500), Some(2));
        assert_eq!(record, [200, 300]);
        assert_eq!(square(&mut record, 0), Some(0));
        assert!(record.is_empty());
    }

    /// A queue with a datagram taken out of its middle. The count is unknown
    /// rather than guessed, and everything is refused until it drains.
    #[test]
    fn a_record_that_does_not_add_up_is_not_squared() {
        let mut record: VecDeque<usize> = [100, 200, 300].into_iter().collect();
        assert_eq!(square(&mut record, 450), None);
        assert_eq!(record, [100, 200, 300], "nothing is dropped on a mismatch");
        assert!(!room_for(&mut record, 450, 1_000, 1, 100));
    }

    /// The accounting the report leaves out, which is the whole reason the
    /// record exists.
    #[test]
    fn every_queued_datagram_is_charged_its_own_accounting() {
        let mut record: VecDeque<usize> = std::iter::repeat_n(1_000, 10).collect();
        let held = 10_000;
        let space = 20_000;
        let free = space + QUEUE_OVERHEAD - 10 * QUEUE_OVERHEAD;
        let wire = 1_000;
        let fits = free / (wire + QUEUE_OVERHEAD);
        assert!(room_for(&mut record, held, space, fits, wire));
        assert!(
            !room_for(&mut record, held, space, fits + 1, wire),
            "the report alone would have allowed {} more",
            space / wire
        );
    }
}
