//! Choosing what to ask the encoder for.
//!
//! The controller says how many bits a second the link will carry. This turns
//! that into an encode size, a declared frame rate and a bitrate.
//!
//! Three knobs, spent in this order. **Bitrate** first, because it is the cheap
//! one. **Frame rate** second, because frame admission drops whole capture
//! frames and the deliverable rates are the output's refresh divided by a whole
//! number: on a 74.983 Hz output that is 75, 37.5, 25, 18.75 and 15. And
//! **resolution** last, because desktop content resists downscaling badly:
//! sharp text degrades far worse under a resample than under coarser
//! quantisation, so sharp text at 25 Hz beats soft text at 75.
//!
//! What it wants and what the encoder holds are separate, and they have to be.
//! Every change costs a key frame drawn from the shared budget in
//! [`crate::idr`], which refuses; a ladder that recorded a change it only
//! *asked* for would believe the encoder was somewhere it never went, and would
//! never ask again.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Bits per pixel per frame the opening point is chosen at.
///
/// Measured rather than assumed: the busiest desktop content measured on this
/// host, a busy quarter-screen window at 1080p60, carried 7.86 Mbit/s of
/// picture, which is 0.063 bits per pixel per frame. Rounded down.
///
/// It is a worst case, and real desktop content measures 0.006 to 0.063, which
/// is why it only ever chooses the point a session *opens* on. After that the
/// ladder closes the loop on what the encoder is really producing.
const OPENING_QUALITY: f64 = 0.06;

/// The coarsest cadence frames are admitted at.
///
/// Five is 15 frames a second on a 75 Hz output, and below that a desktop stops
/// feeling connected.
const MAX_STRIDE: u32 = 5;

/// The share of what the encoder is set to below which the target is retargeted
/// down, and above which it is retargeted up.
///
/// Down is more eager than up because the two errors are not symmetric. Set 25%
/// high, the bucket refuses a fifth of frames and a 75 Hz stream becomes 60.
/// Set 25% low, the picture is slightly soft and nothing else happens. Set
/// twice too high, half the frames go and the session is unusable.
const RETARGET_DOWN: f64 = 0.8;
const RETARGET_UP: f64 = 1.3;

/// The smallest gap between two changes of the operating point.
///
/// Every one costs a full key frame, which on a 2 Mbit/s link is 768 ms of link
/// time, and they are drawn from the same budget a client that lost a frame
/// draws from.
const DWELL: Duration = Duration::from_secs(1);

/// The smallest gap between two changes of the encoded size, which the client
/// must reconfigure its decoder for.
const RESOLUTION_DWELL: Duration = Duration::from_secs(5);

/// Refusals over this window that ask for a step down, as a share of the frames
/// the rate limiter was offered.
const REFUSAL_WINDOW: Duration = Duration::from_secs(2);
const REFUSAL_SHARE: f64 = 0.10;

/// Production under this share of the target over this window asks for a step
/// up.
///
/// Sixty percent because a step from stride `n` to `n - 1` raises production by
/// roughly `n / (n - 1)`, which is 1.5 at the tightest step, so anything above
/// one over 1.5 of the target would step up into a refusal.
const PRODUCTION_WINDOW: Duration = Duration::from_secs(5);
const PRODUCTION_SHARE: f64 = 0.60;

/// An encode size.
///
/// Both dimensions are multiples of sixteen wherever that is free. The encoder
/// codes a height of 1080 as 1088 and attaches a crop, and a presentation path
/// that ignores the crop shows eight rows of working data, so the coded size and
/// the visible size are reported separately rather than assumed equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

impl Size {
    /// Pixels in one frame.
    #[must_use]
    pub const fn pixels(self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

/// A change the ladder wants applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The size to encode at.
    pub size: Size,
    /// The frame rate to declare, in millihertz.
    ///
    /// The encoder budgets bits as the bitrate divided by this and ignores when
    /// frames actually arrive, so it is part of the same decision as the
    /// bitrate rather than an independent knob. The pump delivers at it.
    pub millihertz: u32,
    /// The target, in bits per second.
    pub bitrate: u32,
}

/// What the encoder has done since the session started.
///
/// Cumulative rather than per interval, so a sample missed or delayed still
/// leaves the window exactly right about what it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Produced {
    /// Capture frames the rate limiter was offered.
    pub offered: u64,
    /// Of those, the ones it refused because the picture would have cost more
    /// than the link carries.
    pub refused: u64,
    /// Bits the encoder actually emitted.
    pub bits: u64,
}

/// Tracks the encode settings a session should be using.
#[derive(Debug)]
pub struct Ladder {
    rungs: Vec<Size>,
    /// The output's own refresh, in millihertz, which every declared rate is a
    /// whole fraction of.
    refresh: u32,
    ceiling: u32,
    rung: usize,
    stride: u32,
    bitrate: u32,
    /// What the encoder is known to hold, which is only ever what a caller has
    /// said it applied.
    applied: Settings,
    /// Cumulative counters and when each was taken.
    history: VecDeque<(Instant, Produced)>,
    changed: Option<Instant>,
    resized: Option<Instant>,
}

impl Ladder {
    /// Builds a ladder for a `capture` refreshing `refresh` millihertz, opening
    /// at whatever `target` bits a second can carry and never asking for more
    /// than `ceiling`.
    ///
    /// The rungs are halvings and nothing between them. Measured on this
    /// hardware, scaling to three quarters costs more than not scaling at all:
    /// 9.8 milliseconds against 7.6 at full size, while a halving costs 5.1. A
    /// fraction that is not a power of two puts the scaler on an expensive
    /// filter path, so an intermediate rung would buy a smaller picture for
    /// more latency.
    #[must_use]
    pub fn new(capture: Size, refresh: u32, target: u32, ceiling: u32) -> Self {
        let mut rungs = Vec::new();
        for shift in [2u32, 1, 0] {
            let size = Size {
                width: round_to_even(u32::from(capture.width) >> shift),
                height: round_to_even(u32::from(capture.height) >> shift),
            };
            if size.width >= 256 && size.height >= 144 && !rungs.contains(&size) {
                rungs.push(size);
            }
        }
        if rungs.is_empty() {
            rungs.push(capture);
        }
        let bitrate = target.min(ceiling).max(1);
        let (rung, stride) = opening(&rungs, refresh, bitrate);
        let applied = settings(&rungs, refresh, rung, stride, bitrate);
        Self {
            rungs,
            refresh,
            ceiling,
            rung,
            stride,
            bitrate,
            applied,
            history: VecDeque::new(),
            changed: None,
            resized: None,
        }
    }

    /// The settings the ladder wants, which the encoder may not have taken.
    #[must_use]
    pub fn settings(&self) -> Settings {
        settings(
            &self.rungs,
            self.refresh,
            self.rung,
            self.stride,
            self.bitrate,
        )
    }

    /// The settings the encoder is known to hold.
    #[must_use]
    pub const fn applied(&self) -> Settings {
        self.applied
    }

    /// Records what the caller managed to apply.
    ///
    /// The dwells are stamped here rather than where the decision was made,
    /// because a change that was refused cost no key frame and must not spend
    /// the interval that rations them. The measurement window is cleared with
    /// them: what the encoder produced under the old settings says nothing
    /// about the new ones.
    pub fn acknowledge(&mut self, applied: Settings, now: Instant) {
        if applied.size != self.applied.size {
            self.resized = Some(now);
        }
        if applied != self.applied {
            self.changed = Some(now);
            self.history.clear();
        }
        self.applied = applied;
    }

    /// Feeds the ladder the controller's target and what the encoder has done.
    ///
    /// Returns the settings to apply when they differ from what the encoder is
    /// known to hold, and keeps reporting the same want until it is
    /// acknowledged, so a refused change is asked for again rather than
    /// forgotten.
    pub fn observe(&mut self, target: u32, produced: Produced, now: Instant) -> Option<Settings> {
        self.history.push_back((now, produced));
        while self
            .history
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > PRODUCTION_WINDOW)
            && self.history.len() > 2
        {
            self.history.pop_front();
        }

        let target = target.min(self.ceiling).max(1);
        if self.rested(now) {
            self.retarget(target);
            self.step(target, now);
        }
        let want = self.settings();
        (want != self.applied).then_some(want)
    }

    /// Moves the encoder's operating point when the target has left the band it
    /// was set for.
    ///
    /// The property setter has no equality guard of its own, so writing an
    /// unchanged bitrate still costs a key frame: measured on a running
    /// pipeline, writing 20000 over 20000 produced an IDR. Nothing here writes
    /// one that has not moved outside the band.
    fn retarget(&mut self, target: u32) {
        let held = f64::from(self.bitrate);
        let wanted = f64::from(target);
        if wanted < RETARGET_DOWN * held || wanted > RETARGET_UP * held {
            self.bitrate = target;
        }
    }

    /// Closes the loop on what the rate limiter is actually doing.
    fn step(&mut self, target: u32, now: Instant) {
        if self.refusing() {
            self.down(now);
        } else if self.starved(target) {
            self.up(now);
        }
    }

    /// Whether the rate limiter refused more than its share of the frames it
    /// was offered.
    fn refusing(&self) -> bool {
        let Some((offered, refused, _)) = self.over(REFUSAL_WINDOW) else {
            return false;
        };
        if offered == 0 {
            return false;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame count over two seconds is a few hundred"
        )]
        {
            refused as f64 > REFUSAL_SHARE * offered as f64
        }
    }

    /// Whether the encoder is producing so far under the target that a larger
    /// picture or a faster one would fit.
    fn starved(&self, target: u32) -> bool {
        let Some((_, refused, bits)) = self.over(PRODUCTION_WINDOW) else {
            return false;
        };
        let Some(seconds) = self.span(PRODUCTION_WINDOW) else {
            return false;
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a bit count over five seconds is far below the exact integer range of a double"
        )]
        {
            refused == 0 && bits as f64 / seconds < PRODUCTION_SHARE * f64::from(target)
        }
    }

    /// Frame rate first, resolution last.
    fn down(&mut self, now: Instant) {
        if self.stride < MAX_STRIDE {
            self.stride += 1;
        } else if self.rung > 0 && self.resolution_rested(now) {
            self.rung -= 1;
            self.stride = 1;
        }
    }

    fn up(&mut self, now: Instant) {
        if self.stride > 1 {
            self.stride -= 1;
        } else if self.rung + 1 < self.rungs.len() && self.resolution_rested(now) {
            self.rung += 1;
            // Four times the pixels at half the rate is twice the demand, not
            // four times it, so the climb into a larger rung lands somewhere it
            // has a chance of affording rather than at the top of it.
            self.stride = 2;
        }
    }

    fn rested(&self, now: Instant) -> bool {
        self.changed
            .is_none_or(|last| now.duration_since(last) >= DWELL)
    }

    fn resolution_rested(&self, now: Instant) -> bool {
        self.resized
            .is_none_or(|last| now.duration_since(last) >= RESOLUTION_DWELL)
    }

    /// Offered, refused and bits produced over the last `window`, taken as the
    /// difference between the cumulative counters at each end of it.
    ///
    /// `None` while the history covers less than half of what was asked for: a
    /// window that has not run yet has not measured what it claims to.
    fn over(&self, window: Duration) -> Option<(u64, u64, u64)> {
        let (end, last) = self.history.back()?;
        let (at, first) = self
            .history
            .iter()
            .find(|(at, _)| end.duration_since(*at) <= window)?;
        if end.duration_since(*at) * 2 < window {
            return None;
        }
        Some((
            last.offered.saturating_sub(first.offered),
            last.refused.saturating_sub(first.refused),
            last.bits.saturating_sub(first.bits),
        ))
    }

    /// Seconds the window actually spans.
    fn span(&self, window: Duration) -> Option<f64> {
        let (end, _) = self.history.back()?;
        let (start, _) = self
            .history
            .iter()
            .find(|(at, _)| end.duration_since(*at) <= window)?;
        let span = end.duration_since(*start).as_secs_f64();
        (span > 0.0).then_some(span)
    }
}

/// The largest rung, and the smallest stride, whose worst case fits `target`.
///
/// Worked through at 74.983 Hz on a 3440x1440 capture, this puts 2 Mbit/s at
/// 1720x720 and 25 frames a second, 8 Mbit/s at 3440x1440 and 25, 20 Mbit/s at
/// 3440x1440 and 37.5, and 25 Mbit/s at the full size and rate. Native
/// resolution at 25 frames a second on 8 Mbit/s is the right answer for a
/// desktop, and it is what the session that provoked this design should have
/// produced.
fn opening(rungs: &[Size], refresh: u32, target: u32) -> (usize, u32) {
    for (index, rung) in rungs.iter().enumerate().rev() {
        for stride in 1..=MAX_STRIDE {
            if worst_case(*rung, refresh, stride) <= f64::from(target) {
                return (index, stride);
            }
        }
    }
    (0, MAX_STRIDE)
}

/// Bits a second the busiest desktop content measured would cost at this
/// operating point.
fn worst_case(rung: Size, refresh: u32, stride: u32) -> f64 {
    OPENING_QUALITY * f64::from(rung.pixels()) * declared(refresh, stride)
}

/// The frame rate a stride declares, in frames a second.
fn declared(refresh: u32, stride: u32) -> f64 {
    f64::from(refresh) / 1000.0 / f64::from(stride.max(1))
}

fn settings(rungs: &[Size], refresh: u32, rung: usize, stride: u32, bitrate: u32) -> Settings {
    Settings {
        size: rungs[rung],
        millihertz: (refresh / stride.max(1)).max(1),
        bitrate,
    }
}

/// Rounds down to an even number.
///
/// The crop that H.264 attaches is expressed in chroma samples, half a pixel
/// apart in each axis under 4:2:0, so even is the constraint that matters. The
/// encoder rounds up to whole macroblocks itself and reports the coded size
/// separately.
fn round_to_even(value: u32) -> u16 {
    let clamped = u16::try_from(value.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
    clamped & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output every measurement in `spec/findings.md` was taken on.
    const NATIVE: Size = Size {
        width: 3440,
        height: 1440,
    };
    const REFRESH: u32 = 74_983;
    const CEILING: u32 = 20_000_000;

    fn ladder(target: u32) -> Ladder {
        Ladder::new(NATIVE, REFRESH, target, CEILING)
    }

    /// A session in which the encoder produces exactly `bits` a second and the
    /// rate limiter refuses nothing, sampled four times a second.
    fn steady(ladder: &mut Ladder, target: u32, bits: u64, seconds: u32) -> Vec<Settings> {
        let start = Instant::now();
        let mut produced = Produced::default();
        let mut applied = Vec::new();
        for tick in 1..=seconds * 4 {
            let now = start + Duration::from_millis(u64::from(tick) * 250);
            produced.offered += 19;
            produced.bits += bits / 4;
            if let Some(want) = ladder.observe(target, produced, now) {
                ladder.acknowledge(want, now);
                applied.push(want);
            }
        }
        applied
    }

    #[test]
    fn every_rung_is_a_halving_and_even_in_both_axes() {
        let ladder = ladder(8_000_000);
        for rung in &ladder.rungs {
            assert_eq!(rung.width % 2, 0, "{rung:?}");
            assert_eq!(rung.height % 2, 0, "{rung:?}");
            let ratio = f64::from(NATIVE.width) / f64::from(rung.width);
            assert!(
                (ratio - ratio.round()).abs() < 0.01 && ratio.round().log2().fract() == 0.0,
                "{rung:?} is not a halving of {NATIVE:?}"
            );
            let shape = f64::from(rung.width) / f64::from(rung.height);
            assert!(
                (shape - f64::from(NATIVE.width) / f64::from(NATIVE.height)).abs() < 0.02,
                "{rung:?} would make the scaler stretch the picture"
            );
        }
    }

    #[test]
    fn the_opening_point_is_the_largest_picture_the_target_can_carry() {
        // The worked table in `spec/congestion.md`, on this output.
        for (target, width, fps) in [
            (2_000_000, 1720, 24),
            (8_000_000, 3440, 24),
            (20_000_000, 3440, 37),
            (25_000_000, 3440, 74),
        ] {
            let opened = Ladder::new(NATIVE, REFRESH, target, 40_000_000).settings();
            assert_eq!(opened.size.width, width, "at {target} bit/s");
            assert_eq!(opened.millihertz / 1000, fps, "at {target} bit/s");
        }
    }

    #[test]
    fn a_session_opens_sharp_and_slow_rather_than_soft_and_fast() {
        // Sharp text at 25 Hz beats soft text at 75, and eight megabits buys
        // the whole picture at 25 rather than a quarter of it at 75.
        let opened = ladder(8_000_000).settings();
        assert_eq!(opened.size, NATIVE);
        assert!(opened.millihertz < REFRESH / 2);
    }

    #[test]
    fn a_want_nobody_applied_is_asked_for_again() {
        // Every change costs a key frame out of a budget that refuses. A ladder
        // that recorded what it asked for would believe the encoder had moved
        // and would never ask again, and the two would stay apart for the rest
        // of the session.
        let mut ladder = ladder(2_000_000);
        let now = Instant::now() + DWELL;
        let want = ladder
            .observe(12_000_000, Produced::default(), now)
            .expect("a six-fold rise asks for a change");
        assert_eq!(
            ladder.observe(12_000_000, Produced::default(), now),
            Some(want),
            "a want nobody acknowledged is still a want"
        );
        ladder.acknowledge(want, now);
        assert_eq!(
            ladder.observe(12_000_000, Produced::default(), now),
            None,
            "and once applied it is not asked for again"
        );
        assert_eq!(ladder.applied(), want);
    }

    #[test]
    fn the_bitrate_moves_only_when_it_leaves_the_band() {
        let mut ladder = ladder(8_000_000);
        let start = Instant::now();
        let held = ladder.applied().bitrate;

        // Writing an unchanged bitrate is not free: the setter has no equality
        // guard, so the write alone costs a key frame.
        for target in [held * 9 / 10, held * 6 / 5] {
            assert_eq!(
                ladder
                    .observe(target, Produced::default(), start + DWELL)
                    .map(|want| want.bitrate),
                None,
                "{target} is inside the band around {held}"
            );
        }
        assert_eq!(
            ladder
                .observe(held / 2, Produced::default(), start + DWELL)
                .map(|want| want.bitrate),
            Some(held / 2),
            "a link that halved must be followed"
        );
    }

    #[test]
    fn the_ladder_never_asks_for_more_than_the_ceiling() {
        let mut ladder = ladder(2_000_000);
        for applied in steady(&mut ladder, u32::MAX, 40_000_000, 60) {
            assert!(applied.bitrate <= CEILING, "{applied:?}");
        }
        assert_eq!(ladder.applied().bitrate, CEILING);
    }

    /// The rate limiter refusing a fifth of what it is offered, sampled four
    /// times a second, for `seconds`.
    fn refusing(ladder: &mut Ladder, target: u32, seconds: u32) -> Vec<Settings> {
        let start = Instant::now();
        let mut produced = Produced::default();
        let mut applied = Vec::new();
        for tick in 1..=seconds * 4 {
            let now = start + Duration::from_millis(u64::from(tick) * 250);
            produced.offered += 19;
            produced.refused += 4;
            produced.bits += 250_000;
            if let Some(want) = ladder.observe(target, produced, now) {
                ladder.acknowledge(want, now);
                applied.push(want);
            }
        }
        applied
    }

    #[test]
    fn refusals_step_the_frame_rate_down_before_the_resolution() {
        let mut ladder = Ladder::new(NATIVE, REFRESH, 25_000_000, 40_000_000);
        assert_eq!(ladder.applied().size, NATIVE, "the whole picture at 75 Hz");
        let opened = ladder.applied().millihertz;

        let applied = refusing(&mut ladder, 25_000_000, 4);
        assert!(
            applied.iter().all(|want| want.size == NATIVE),
            "the picture shrank while the frame rate still had room: {applied:?}"
        );
        assert!(
            ladder.applied().millihertz * 4 <= opened,
            "four steps of stride must be spent first, and left {}",
            ladder.applied().millihertz
        );
    }

    #[test]
    fn a_link_that_refuses_even_the_slowest_rate_gives_up_the_picture() {
        let mut ladder = Ladder::new(NATIVE, REFRESH, 25_000_000, 40_000_000);
        let applied = refusing(&mut ladder, 25_000_000, 12);
        let shrank = applied.iter().position(|want| want.size != NATIVE);
        assert!(
            shrank.is_some_and(|at| at >= 4),
            "the picture must be the last thing spent, and went at change {shrank:?}"
        );
        assert!(ladder.applied().size.pixels() < NATIVE.pixels());
    }

    #[test]
    fn a_link_with_room_to_spare_climbs_back_to_the_whole_picture() {
        let mut ladder = ladder(2_000_000);
        assert!(ladder.applied().size.pixels() < NATIVE.pixels());
        // Twenty megabits, and an encoder producing four, which is well under
        // the sixty percent that says a larger picture would fit.
        steady(&mut ladder, 20_000_000, 4_000_000, 120);
        assert_eq!(
            ladder.applied().size,
            NATIVE,
            "a link that carries the whole picture must be used for it"
        );
    }

    #[test]
    fn an_encoder_spending_its_whole_budget_is_left_alone() {
        let mut ladder = Ladder::new(NATIVE, REFRESH, 8_000_000, CEILING);
        let opened = ladder.applied();
        let changes = steady(&mut ladder, 8_000_000, 8_000_000, 60);
        assert!(
            changes.is_empty(),
            "a session spending its budget must not be reconfigured: {changes:?}"
        );
        assert_eq!(ladder.applied(), opened);
    }

    #[test]
    fn a_jittering_target_does_not_retarget_the_encoder_on_every_tick() {
        // Every change costs a key frame, drawn from the same budget a client
        // that lost a frame draws from.
        let mut ladder = ladder(12_000_000);
        let start = Instant::now();
        let mut produced = Produced::default();
        let mut changes = 0;
        for tick in 1..=240u32 {
            let now = start + Duration::from_millis(u64::from(tick) * 250);
            produced.offered += 19;
            produced.bits += 3_000_000;
            let target = 12_000_000 + (tick.wrapping_mul(7919) % 240_000);
            if let Some(want) = ladder.observe(target, produced, now) {
                ladder.acknowledge(want, now);
                changes += 1;
            }
        }
        assert!(
            changes <= 8,
            "a minute on an unchanging link cost {changes} key frames"
        );
    }
}
