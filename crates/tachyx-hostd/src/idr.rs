//! Rationing forced key frames.
//!
//! Three separate things force a key frame: a bitrate change, a resolution
//! change, and a client asking for one after losing a frame. This encoder offers
//! neither intra refresh nor long-term reference frames, so each of those costs a
//! full key frame, and a key frame is large: 130 kB at 1080p against 17 kB for an
//! ordinary frame.
//!
//! That closes a loop. Congestion causes a bitrate step down, the step forces a
//! key frame, the key frame is a burst delivered exactly when there is no room
//! for one, the burst causes loss, the loss causes a client request, and the
//! request forces another key frame.
//!
//! Rate-limiting each cause separately does not break the loop, because three
//! limiters that each allow one per second allow three per second. They draw
//! from one budget instead.

use std::time::{Duration, Instant};

/// Why a key frame was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The session is starting and the client has nothing to decode from.
    Startup,
    /// The encoder's target bitrate changed.
    Bitrate,
    /// The encoded resolution changed.
    Resolution,
    /// The client lost a frame and cannot continue from what it has.
    ClientRequest,
}

/// What a request for a key frame is worth against the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Emit one.
    Allow,
    /// Refuse. The caller must not apply a change that would force one anyway.
    Refuse,
}

/// A shared allowance for every cause of a key frame.
#[derive(Debug)]
pub struct Budget {
    last: Option<Instant>,
    /// Bytes of the most recent key frame, measured rather than assumed.
    key_frame_bytes: u32,
    /// The encoder's current target, in bits per second.
    target_bits: u32,
}

impl Budget {
    /// The share of the link a key frame may occupy on average.
    ///
    /// Above this the key frames themselves become the congestion.
    const SHARE: f32 = 0.15;

    /// Longest gap the budget will ever impose, however low the target falls.
    const MAX_INTERVAL: Duration = Duration::from_secs(4);

    /// Starts a budget for a session at `target_bits` per second.
    #[must_use]
    pub const fn new(target_bits: u32) -> Self {
        Self {
            last: None,
            // Replaced by measurement after the first key frame. Until then this
            // stands in for a 1080p key frame.
            key_frame_bytes: 130_000,
            target_bits,
        }
    }

    /// Records the size of a key frame the encoder actually produced.
    pub const fn observe(&mut self, bytes: u32) {
        self.key_frame_bytes = bytes;
    }

    /// Records a change in the encoder's target bitrate.
    pub const fn retarget(&mut self, target_bits: u32) {
        self.target_bits = target_bits;
    }

    /// The shortest interval between key frames the current target can carry.
    fn interval(&self) -> Duration {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a bitrate is far below the exact integer range of a float"
        )]
        let allowance = Self::SHARE * self.target_bits as f32;
        if allowance <= 0.0 {
            return Self::MAX_INTERVAL;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame size is far below the exact integer range of a float"
        )]
        let seconds = (self.key_frame_bytes as f32 * 8.0) / allowance;
        Duration::from_secs_f32(seconds).min(Self::MAX_INTERVAL)
    }

    /// Asks whether a key frame may be emitted now.
    ///
    /// A session with nothing to decode is never refused: refusing there would
    /// leave the client with a blank screen indefinitely.
    pub fn request(&mut self, reason: Reason, now: Instant) -> Verdict {
        let allowed = reason == Reason::Startup
            || self
                .last
                .is_none_or(|last| now.duration_since(last) >= self.interval());
        if allowed {
            self.last = Some(now);
            Verdict::Allow
        } else {
            Verdict::Refuse
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eight megabits per second, the target where the loop was measured.
    const TARGET: u32 = 8_000_000;

    #[test]
    fn a_starting_session_is_never_refused() {
        let mut budget = Budget::new(TARGET);
        let now = Instant::now();
        budget.request(Reason::ClientRequest, now);
        assert_eq!(
            budget.request(Reason::Startup, now),
            Verdict::Allow,
            "refusing here leaves the client with nothing to decode"
        );
    }

    #[test]
    fn every_cause_draws_from_one_budget() {
        let mut budget = Budget::new(TARGET);
        let now = Instant::now();
        assert_eq!(budget.request(Reason::Bitrate, now), Verdict::Allow);
        for reason in [Reason::Resolution, Reason::ClientRequest, Reason::Bitrate] {
            assert_eq!(
                budget.request(reason, now),
                Verdict::Refuse,
                "{reason:?} must not have an allowance of its own"
            );
        }
    }

    #[test]
    fn a_narrower_link_spaces_key_frames_further_apart() {
        let mut generous = Budget::new(TARGET);
        let mut narrow = Budget::new(TARGET / 8);
        assert!(
            narrow.interval() > generous.interval(),
            "the budget must widen as the link narrows"
        );
        let now = Instant::now();
        generous.request(Reason::Bitrate, now);
        narrow.request(Reason::Bitrate, now);

        let later = now + generous.interval();
        assert_eq!(generous.request(Reason::Bitrate, later), Verdict::Allow);
        assert_eq!(narrow.request(Reason::Bitrate, later), Verdict::Refuse);
    }

    #[test]
    fn the_budget_follows_the_measured_key_frame_size() {
        let mut budget = Budget::new(TARGET);
        let assumed = budget.interval();
        budget.observe(260_000);
        assert!(
            budget.interval() > assumed,
            "a key frame twice the assumed size must be allowed half as often"
        );
    }

    #[test]
    fn the_budget_never_starves_a_session_entirely() {
        let mut budget = Budget::new(1);
        assert!(budget.interval() <= Budget::MAX_INTERVAL);
        let now = Instant::now();
        budget.request(Reason::Startup, now);
        assert_eq!(
            budget.request(Reason::ClientRequest, now + Budget::MAX_INTERVAL),
            Verdict::Allow
        );
    }
}
