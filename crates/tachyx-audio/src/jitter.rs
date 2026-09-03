//! What to play next, and how deep the sound waiting to be played is.
//!
//! The path delivers sound in clumps and the speaker consumes it evenly, so
//! something has to hold a little of it back. This is that: a queue measured in
//! frames of sound rather than in packets or in bytes, because a hole in the
//! stream is frames of sound that have to be invented and takes exactly as long
//! to play as the packets that were lost.
//!
//! Nothing here decodes, waits or touches a clock. It is the policy alone, so a
//! test can play an hour of a badly behaved link through it in no time at all.

use std::collections::VecDeque;

use tachyx_client::AudioEvent;

use crate::stats::Stats;

/// How much sound is gathered before any of it is played, in frames.
///
/// Two, which is twenty milliseconds at the frame size the host encodes. The
/// buffer is what a packet that arrives late is paid for out of, so this is the
/// lateness the path is forgiven, and it is added to the latency of every packet
/// whether it needed it or not. Twenty milliseconds is under the threshold where
/// a person hears sound as trailing the picture, and the picture on this path is
/// tens of milliseconds behind the host's screen in any case.
const TARGET: usize = 2;

/// How deep the buffer may get before the oldest frame is thrown away.
///
/// Ten frames, which is a hundred milliseconds. Depth past the target is latency
/// nobody asked for, and it does not drain by itself: the host sends at the rate
/// the speaker plays, so whatever a burst or a slow clock leaves in the buffer
/// stays there for the rest of the session unless something takes it out. This
/// is what takes it out.
///
/// The height above [`TARGET`] is the burst a radio in power save is forgiven.
/// It wakes, delivers what it was holding in one clump, and the clump is sound
/// that arrived rather than sound that was lost, so it is worth eighty
/// milliseconds of buffer to keep it. Past that the choice is between latency
/// nothing will ever remove and a gap of a few frames, and the gap is the one a
/// listener stops noticing.
const CEILING: usize = 10;

/// The most sound that may wait, whatever is happening.
///
/// Two seconds. Nothing should ever reach it: the ceiling above holds the buffer
/// at sixty milliseconds while anything is playing. It is there for the case
/// where the thread that plays has stopped, so that a session goes on costing
/// nothing rather than growing until the phone kills it.
const LIMIT: usize = 200;

/// What the decoder does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Next {
    /// Decode this packet.
    Packet(Vec<u8>),
    /// Invent one frame of sound.
    ///
    /// The packet behind the hole comes with it when there is one, because the
    /// host encodes with forward error correction: that packet carries a coarse
    /// copy of the frame the hole swallowed, and a copy is worth more than
    /// anything the decoder can guess.
    Conceal(Option<Vec<u8>>),
}

/// The sound waiting to be played.
#[derive(Debug, Default)]
pub(crate) struct Jitter {
    queue: VecDeque<AudioEvent>,
    /// The depth of `queue` in frames, which is not its length: one
    /// [`AudioEvent::Lost`] stands for as many frames as it says.
    frames: usize,
    counted: Stats,
}

impl Jitter {
    /// Takes one thing the session decided, in the order it was captured.
    pub(crate) fn push(&mut self, event: AudioEvent) {
        self.frames += frames(&event);
        self.queue.push_back(event);
        while self.frames > LIMIT {
            self.forget();
        }
    }

    /// How much sound is waiting, in frames.
    pub(crate) const fn depth(&self) -> usize {
        self.frames
    }

    /// Whether there is enough sound waiting to start playing.
    pub(crate) const fn ready(&self) -> bool {
        self.frames >= TARGET
    }

    /// The next frame to play, or `None` when the buffer has run dry.
    ///
    /// One frame per call, because the caller writes one frame per call: the
    /// device is what decides how fast this is asked.
    pub(crate) fn take(&mut self) -> Option<Next> {
        self.rebase();
        match self.queue.front_mut()? {
            // A hole longer than one frame is walked a frame at a time. Only its
            // last frame can be rebuilt from the packet behind it, so the rest
            // are guessed.
            AudioEvent::Lost(missing) if *missing > 1 => {
                *missing -= 1;
                self.frames -= 1;
                self.counted.concealed += 1;
                Some(Next::Conceal(None))
            }
            AudioEvent::Lost(_) => {
                self.queue.pop_front();
                self.frames -= 1;
                let repair = self.queue.front().and_then(|behind| match behind {
                    AudioEvent::Packet { opus, .. } => Some(opus.clone()),
                    AudioEvent::Lost(_) => None,
                });
                if repair.is_some() {
                    self.counted.recovered += 1;
                } else {
                    self.counted.concealed += 1;
                }
                Some(Next::Conceal(repair))
            }
            AudioEvent::Packet { .. } => {
                let Some(AudioEvent::Packet { opus, .. }) = self.queue.pop_front() else {
                    return None;
                };
                self.frames -= 1;
                self.counted.played += 1;
                Some(Next::Packet(opus))
            }
        }
    }

    /// Records that the buffer ran dry with the device still asking.
    pub(crate) const fn starved(&mut self) {
        self.counted.underruns += 1;
    }

    /// Records a frame the decoder invented outside [`Jitter::take`], which is
    /// every frame played while the buffer is refilling.
    pub(crate) const fn invented(&mut self) {
        self.counted.concealed += 1;
    }

    /// What the path has done, as of now.
    pub(crate) fn stats(&self) -> Stats {
        Stats {
            buffered: u32::try_from(self.frames).unwrap_or(u32::MAX),
            ..self.counted
        }
    }

    /// Throws away the oldest frame while the buffer is over the ceiling.
    ///
    /// One frame a call, so the correction is spread over as many frames as the
    /// buffer is deep rather than cutting a chunk out of the sound at once. At
    /// the drift two sound cards really have, this runs once every few minutes.
    fn rebase(&mut self) {
        if self.frames > CEILING {
            self.forget();
            self.counted.dropped += 1;
        }
    }

    /// Drops the oldest frame of sound, whatever it is.
    fn forget(&mut self) {
        match self.queue.front_mut() {
            Some(AudioEvent::Lost(missing)) if *missing > 1 => *missing -= 1,
            Some(_) => {
                self.queue.pop_front();
            }
            // Nothing to forget, which the callers' own conditions already rule
            // out: both ask only when the buffer holds frames.
            None => return,
        }
        self.frames -= 1;
    }
}

/// How many frames of sound one event stands for.
fn frames(event: &AudioEvent) -> usize {
    match event {
        AudioEvent::Packet { .. } => 1,
        AudioEvent::Lost(missing) => usize::try_from(*missing).unwrap_or(usize::MAX),
    }
}

#[cfg(test)]
mod tests {
    use tachyx_proto::media::{FrameId, Timestamp};

    use super::*;

    fn packet(id: u32) -> AudioEvent {
        AudioEvent::Packet {
            id: FrameId(id),
            capture: Timestamp::from_session_micros(u64::from(id) * 10_000),
            opus: vec![u8::try_from(id % 251).expect("under a byte"); 8],
        }
    }

    fn sound(jitter: &mut Jitter, ids: std::ops::Range<u32>) {
        for id in ids {
            jitter.push(packet(id));
        }
    }

    #[test]
    fn nothing_plays_until_there_is_enough_to_play_from() {
        let mut jitter = Jitter::default();
        jitter.push(packet(0));
        assert!(!jitter.ready(), "one frame is not a buffer");
        jitter.push(packet(1));
        assert!(jitter.ready());
    }

    #[test]
    fn packets_come_out_one_frame_at_a_time_in_order() {
        let mut jitter = Jitter::default();
        sound(&mut jitter, 0..3);
        assert_eq!(jitter.depth(), 3);
        for id in 0..3 {
            let Some(Next::Packet(opus)) = jitter.take() else {
                panic!("frame {id} is a packet that arrived");
            };
            assert_eq!(opus[0], u8::try_from(id).expect("under a byte"));
        }
        assert_eq!(jitter.take(), None);
        assert_eq!(jitter.stats().played, 3);
    }

    #[test]
    fn the_packet_behind_a_hole_repairs_it() {
        let mut jitter = Jitter::default();
        jitter.push(AudioEvent::Lost(1));
        jitter.push(packet(1));
        assert_eq!(
            jitter.take(),
            Some(Next::Conceal(Some(vec![1; 8]))),
            "the host encodes a copy of the last frame into every packet"
        );
        assert!(matches!(jitter.take(), Some(Next::Packet(_))));
        let stats = jitter.stats();
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.concealed, 0, "nothing had to be guessed");
    }

    #[test]
    fn a_hole_longer_than_one_frame_is_guessed_but_for_its_last_frame() {
        let mut jitter = Jitter::default();
        jitter.push(AudioEvent::Lost(3));
        jitter.push(packet(3));
        assert_eq!(jitter.take(), Some(Next::Conceal(None)));
        assert_eq!(jitter.take(), Some(Next::Conceal(None)));
        assert_eq!(
            jitter.take(),
            Some(Next::Conceal(Some(vec![3; 8]))),
            "one packet carries one copy, so only the frame next to it is rebuilt"
        );
        assert!(matches!(jitter.take(), Some(Next::Packet(_))));
        let stats = jitter.stats();
        assert_eq!(stats.concealed, 2);
        assert_eq!(stats.recovered, 1);
    }

    #[test]
    fn a_hole_with_nothing_behind_it_yet_is_guessed() {
        let mut jitter = Jitter::default();
        jitter.push(AudioEvent::Lost(1));
        assert_eq!(
            jitter.take(),
            Some(Next::Conceal(None)),
            "there is no packet in hand to take a copy out of"
        );
    }

    #[test]
    fn a_buffer_the_host_is_filling_faster_than_the_speaker_empties_it_is_held_down() {
        let mut jitter = Jitter::default();
        sound(&mut jitter, 0..20);
        let mut played = 0;
        // The speaker asks ten times; the host put twenty frames in.
        for _ in 0..10 {
            if let Some(Next::Packet(_)) = jitter.take() {
                played += 1;
            }
        }
        assert!(
            jitter.depth() <= CEILING,
            "the buffer has to come back down to the ceiling: {} deep",
            jitter.depth()
        );
        assert_eq!(played, 10, "every frame handed out was still real sound");
        assert_eq!(
            jitter.stats().dropped,
            5,
            "one frame a call, rather than a chunk of sound at once"
        );
    }

    #[test]
    fn sound_nothing_is_playing_cannot_grow_without_bound() {
        let mut jitter = Jitter::default();
        sound(&mut jitter, 0..1_000);
        assert_eq!(
            jitter.depth(),
            LIMIT,
            "a player that stopped must not cost the session its memory"
        );
    }
}
