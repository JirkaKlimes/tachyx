//! Sound out of the datagrams that carried it, put back in order.
//!
//! One Opus packet is one datagram, so there is no reassembly here and nothing
//! to repair: a packet either arrived or it did not. What is left is the order,
//! which the path is free to disturb, and the gaps, which are the only record
//! that a packet existed at all.
//!
//! Nothing in this module decodes anything. It hands the player packets in the
//! order they were captured, and says how much sound is missing between two of
//! them, because concealing a gap needs its length and the decoder cannot know
//! it.

use std::collections::{BTreeMap, VecDeque};

use tachyx_proto::media::{FrameId, Timestamp};
use tachyx_proto::packet::AudioPacket;

/// How many packets have to pile up past a hole before the hole is real.
///
/// Two, so a packet that overtook one neighbour is put back in its place and a
/// packet that overtook two is concealed instead. It bounds the memory as well:
/// a hole is decided on the packet that makes this many, so the queue never
/// holds more than one packet that is not due.
///
/// The wait is paid on holes alone. A stream arriving in order is handed on the
/// instant each packet lands, because the packet that is due is the packet that
/// arrived. Only a hole makes the queue wait, and then it waits for one further
/// packet, which is ten milliseconds at the frame size the host encodes. That is
/// the whole trade: ten milliseconds of delay behind a hole, against turning
/// every reordered packet into a concealed one and then a discarded one, which
/// is two audible faults for a path that did nothing worse than deliver two
/// packets the wrong way round.
const PROVES_LOST: usize = 2;

/// What the player does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    /// One Opus packet, in the order it was captured.
    Packet {
        /// Which packet of the session's sound this is.
        id: FrameId,
        /// When the host captured the samples.
        capture: Timestamp,
        /// The Opus packet itself.
        ///
        /// Owned, because the datagram it arrived in is gone by the time the
        /// player asks. It is the one copy the sound path makes.
        opus: Vec<u8>,
    },
    /// This many packets never arrived, and the sound they held has to be
    /// invented before the next packet is played.
    Lost(u32),
}

/// The sound that has arrived, and the holes between it.
///
/// It counts nothing and measures nothing: what a player did with the events is
/// the player's own to report.
#[derive(Debug, Default)]
pub struct AudioQueue {
    /// The identifier the next event carries. `None` until the first packet of
    /// the session arrives, which is what sets where the count begins.
    next: Option<u32>,
    /// Packets that arrived ahead of one still missing.
    ///
    /// It holds fewer than [`PROVES_LOST`] of them: the packet that would make
    /// it that large is the packet that proves the missing one gone.
    held: BTreeMap<u32, (Timestamp, Vec<u8>)>,
    ready: VecDeque<AudioEvent>,
}

impl AudioQueue {
    /// Takes one packet a datagram carried.
    ///
    /// The identifier counts the session's Opus packets and skips none, so a
    /// hole in it is sound that was lost. It counts at a hundred a second and
    /// is thirty two bits wide, which is over a year of a single session, so
    /// nothing here reasons about it wrapping.
    pub fn push(&mut self, packet: AudioPacket<'_>) {
        let id = packet.id.0;
        if self.next.is_some_and(|next| id < next) {
            // Either a duplicate or a packet that arrived after the hole it
            // left was concealed. Playing it now would put its sound after the
            // sound invented to replace it.
            return;
        }
        self.held.insert(id, (packet.capture, packet.opus.to_vec()));
        // The first packet of a session begins the count wherever it happens to
        // be. Nothing before it was lost: a client hears the sound from where it
        // joined, not the sound it was not there for.
        self.next.get_or_insert(id);
        self.drain();
    }

    /// The next thing to play, in order.
    pub fn pop(&mut self) -> Option<AudioEvent> {
        self.ready.pop_front()
    }

    /// Moves everything that is decided into the ready queue.
    ///
    /// A packet is decided when it is the one due, and a hole is decided when
    /// enough packets past it have arrived to prove that nothing will fill it.
    fn drain(&mut self) {
        let Some(mut next) = self.next else {
            return;
        };
        loop {
            while let Some((capture, opus)) = self.held.remove(&next) {
                self.ready.push_back(AudioEvent::Packet {
                    id: FrameId(next),
                    capture,
                    opus,
                });
                next += 1;
            }
            match self.held.keys().next() {
                Some(&nearest) if self.held.len() >= PROVES_LOST => {
                    self.ready.push_back(AudioEvent::Lost(nearest - next));
                    next = nearest;
                }
                // Either nothing is waiting, or too little of it to say that
                // what is missing is lost rather than late.
                _ => break,
            }
        }
        self.next = Some(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(id: u32) -> Vec<u8> {
        vec![u8::try_from(id % 251).expect("under a byte"); 8]
    }

    /// One datagram's worth of sound, as the host would have sent it.
    fn sound(id: u32) -> (u32, Vec<u8>) {
        (id, packet(id))
    }

    fn push(queue: &mut AudioQueue, (id, opus): &(u32, Vec<u8>)) {
        queue.push(AudioPacket {
            id: FrameId(*id),
            capture: Timestamp::from_session_micros(u64::from(*id) * 10_000),
            opus,
        });
    }

    fn drained(queue: &mut AudioQueue) -> Vec<AudioEvent> {
        std::iter::from_fn(|| queue.pop()).collect()
    }

    #[test]
    fn sound_that_arrives_in_order_is_handed_on_at_once() {
        let mut queue = AudioQueue::default();
        for id in 0..4 {
            push(&mut queue, &sound(id));
            let out = drained(&mut queue);
            assert_eq!(
                out,
                vec![AudioEvent::Packet {
                    id: FrameId(id),
                    capture: Timestamp::from_session_micros(u64::from(id) * 10_000),
                    opus: packet(id),
                }],
                "an unbroken stream waits for nothing"
            );
        }
    }

    #[test]
    fn a_session_joined_part_way_through_lost_nothing() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(9_000));
        assert!(
            matches!(drained(&mut queue).as_slice(), [AudioEvent::Packet { .. }]),
            "the first packet sets the count rather than proving nine thousand losses"
        );
    }

    #[test]
    fn a_hole_is_measured_and_reported_once() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(0));
        assert_eq!(drained(&mut queue).len(), 1);

        // Two and three are lost. Four alone is not proof of that: it could
        // have overtaken them. Five is.
        push(&mut queue, &sound(1));
        push(&mut queue, &sound(4));
        assert_eq!(
            drained(&mut queue),
            vec![AudioEvent::Packet {
                id: FrameId(1),
                capture: Timestamp::from_session_micros(10_000),
                opus: packet(1),
            }],
            "the packet after the hole waits to see whether the hole is real"
        );

        push(&mut queue, &sound(5));
        let out = drained(&mut queue);
        assert_eq!(out[0], AudioEvent::Lost(2), "two packets of sound are gone");
        assert!(
            matches!(out[1], AudioEvent::Packet { id: FrameId(4), .. }),
            "and the sound behind the hole follows it: {out:?}"
        );
        assert!(matches!(out[2], AudioEvent::Packet { id: FrameId(5), .. }));
    }

    #[test]
    fn two_packets_the_path_swapped_are_put_back() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(0));
        assert_eq!(drained(&mut queue).len(), 1);

        push(&mut queue, &sound(2));
        assert!(
            drained(&mut queue).is_empty(),
            "the packet that overtook its neighbour waits for it"
        );
        push(&mut queue, &sound(1));
        let out = drained(&mut queue);
        assert!(
            matches!(
                out.as_slice(),
                [
                    AudioEvent::Packet { id: FrameId(1), .. },
                    AudioEvent::Packet { id: FrameId(2), .. }
                ]
            ),
            "both are played, in the order they were captured: {out:?}"
        );
    }

    #[test]
    fn a_packet_that_arrives_after_its_hole_was_concealed_is_thrown_away() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(0));
        assert_eq!(drained(&mut queue).len(), 1);
        for id in [3, 4] {
            push(&mut queue, &sound(id));
        }
        let out = drained(&mut queue);
        assert_eq!(out[0], AudioEvent::Lost(2));

        push(&mut queue, &sound(1));
        assert!(
            drained(&mut queue).is_empty(),
            "its place in the sound has already been filled with an invention"
        );
    }

    #[test]
    fn a_duplicate_is_not_played_twice() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(0));
        push(&mut queue, &sound(0));
        assert_eq!(drained(&mut queue).len(), 1);
    }

    #[test]
    fn a_burst_that_swallowed_a_second_of_sound_is_one_hole() {
        let mut queue = AudioQueue::default();
        push(&mut queue, &sound(0));
        assert_eq!(drained(&mut queue).len(), 1);
        for id in 100..102 {
            push(&mut queue, &sound(id));
        }
        let out = drained(&mut queue);
        assert_eq!(
            out[0],
            AudioEvent::Lost(99),
            "the whole outage is one number, and what to do about it is the player's: {out:?}"
        );
        assert_eq!(out.len(), 3);
    }
}
