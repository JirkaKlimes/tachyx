//! Where the client believes the host's pointer is.
//!
//! The pointer is drawn locally and moved locally. A touch surface whose
//! pointer waited for the host to confirm each step would put a round trip
//! inside every gesture, and on a link with any latency at all that is what
//! makes a trackpad feel broken rather than remote.
//!
//! So the finger moves the pointer at once, and the host's reports correct it.
//! Correcting is not the same as obeying: a report describes the host as it was
//! about half a round trip ago, so everything the client has done since has to
//! be replayed on top of it before the two positions describe the same instant.
//! Without that replay a steady drag rubber-bands, one report per frame.
//!
//! Everything here is a fraction of the captured screen, `0.0` at the left or
//! top edge and `1.0` at the right or bottom. The host reports in the pixels of
//! its screen and the client moves in pixels of the streamed picture; those two
//! change independently, and every step of the resolution ladder would
//! otherwise change what a number here means in the middle of a session.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tachyx_proto::control::{CursorImage, CursorPoint, CursorReport, CursorUpdate};

/// How far the client's position may disagree with the host's before the
/// disagreement is treated as a fact rather than as drift.
///
/// Five percent of the screen. Below it the two are the same pointer and the
/// difference is rounding, the host's own acceleration, or an edge one of them
/// reached first; above it something moved the pointer that this client did
/// not, and easing towards it would drag a visible ghost across the screen.
const SNAP: f32 = 0.05;

/// How much of the remaining disagreement each report removes.
///
/// At sixty reports a second a third of the error a report leaves the rest of
/// it gone inside a tenth of a second, which is below what an eye follows, and
/// it is far enough from one that a single wrong report cannot jump the
/// pointer.
const BLEND: f32 = 0.35;

/// How long a local movement is kept, so that it can be replayed on top of a
/// report older than it.
///
/// Half a second is two orders of magnitude more than the round trip this is
/// used across, and a report older than that is not worth correcting from.
const REPLAY_WINDOW: Duration = Duration::from_millis(500);

/// The smallest movement this client has to have asked for before the host's
/// answer to it is worth measuring.
///
/// Four thousandths of the screen, which is fourteen pixels of a 3440-pixel
/// one. Below that the reported position is a handful of integers and the ratio
/// of two of them is mostly rounding.
const GAIN_SAMPLE: f32 = 0.004;

/// The band a measured gain has to fall in to be believed.
///
/// A sample outside it is not a scale factor, it is a report that crossed a
/// pointer the user also moved by hand, or an edge that clipped one of the two
/// movements being compared.
const GAIN_BAND: std::ops::RangeInclusive<f32> = 0.05..=20.0;

/// How much of each measurement the gain estimate takes.
///
/// The first one is taken whole, because starting from the documented ratio and
/// easing towards the real one would spend the first second of every session
/// drawing the pointer in the wrong place.
const GAIN_BLEND: f32 = 0.15;

/// A place on the host's screen, as a fraction of it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Spot {
    /// `0.0` at the left edge, `1.0` at the right.
    pub x: f32,
    /// `0.0` at the top edge, `1.0` at the bottom.
    pub y: f32,
}

impl Spot {
    /// The middle of the screen, which is where a pointer nobody has reported
    /// yet is assumed to be.
    pub const CENTRE: Self = Self { x: 0.5, y: 0.5 };

    const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn plus(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }

    fn minus(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y)
    }

    fn scaled(self, by: f32) -> Self {
        Self::new(self.x * by, self.y * by)
    }

    fn onto_screen(self) -> Self {
        Self::new(self.x.clamp(0.0, 1.0), self.y.clamp(0.0, 1.0))
    }

    fn distance(self, other: Self) -> f32 {
        self.minus(other).length()
    }

    fn length(self) -> f32 {
        self.x.hypot(self.y)
    }
}

/// How much the host has confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tracking {
    /// The host has not said anything about the pointer yet.
    #[default]
    Silent,
    /// The host reports the pointer, and the position has been corrected.
    Reported,
    /// The host cannot report the pointer, so nothing corrects the client's own
    /// reckoning and it drifts. Said out loud rather than hidden, because a
    /// pointer that is wrong and says so is usable and one that is wrong
    /// quietly is not.
    DeadReckoned,
}

/// The picture to draw and where its hotspot sits, in fractions of the screen.
#[derive(Debug, Clone)]
pub struct CursorShape {
    /// Bumped whenever [`CursorShape::image`] changes, so that whatever decodes
    /// the picture decodes each one once.
    pub serial: u64,
    /// The rows, as the host sent them.
    pub image: Arc<CursorImage>,
    /// The picture's width, as a fraction of the screen's width.
    pub width: f32,
    /// Its height, as a fraction of the screen's height.
    pub height: f32,
    /// How far left of the hotspot the picture starts, as a fraction of the
    /// screen's width.
    pub hotspot_x: f32,
    /// How far above the hotspot it starts, as a fraction of the screen's
    /// height.
    pub hotspot_y: f32,
}

/// Where to draw the pointer and what to draw there.
#[derive(Debug, Clone)]
pub struct CursorView {
    /// The hotspot, as a fraction of the picture.
    pub at: Spot,
    /// The picture, or `None` when the host has sent none and the client must
    /// draw a pointer of its own.
    pub shape: Option<CursorShape>,
    /// How much of this the host has confirmed.
    pub tracking: Tracking,
}

/// Keeps the client's own idea of where the host's pointer is.
#[derive(Debug)]
pub struct CursorTracker {
    predicted: Spot,
    /// Movements this client made, oldest first, each with the clock at the
    /// moment it was applied.
    applied: VecDeque<(Instant, Spot)>,
    shape: Option<CursorShape>,
    tracking: Tracking,
    /// Whether the host says the pointer is over the captured screen. True
    /// until it says otherwise, so a pointer nobody has reported is drawn.
    on_screen: bool,
    /// What one unit of asked-for movement actually moves the host's pointer.
    ///
    /// The wire carries a displacement in pixels of the streamed picture and
    /// says the host scales it into its own. By how much, the host never says,
    /// and it is not one thing: the rung of the resolution ladder the stream is
    /// on, the output's own scale factor and whatever pointer acceleration the
    /// compositor applies all multiply into it, and all of them change under a
    /// running session. So it is measured from the reports rather than assumed,
    /// starting at the ratio the wire documents.
    gain: f32,
    /// Whether `gain` is a measurement rather than that starting assumption.
    measured: bool,
    /// The previous report: when it was taken, where it put the pointer, and
    /// whether that was clear of the screen's edges.
    last: Option<(Instant, Spot, bool)>,
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self {
            predicted: Spot::CENTRE,
            applied: VecDeque::new(),
            shape: None,
            tracking: Tracking::default(),
            on_screen: true,
            gain: 1.0,
            measured: false,
            last: None,
        }
    }
}

impl CursorTracker {
    /// A tracker for a pointer nobody has reported, assumed to be in the middle
    /// of the screen.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where to draw the pointer, or `None` when there is nothing to draw.
    ///
    /// Nothing to draw means one of two things: the host has not said anything
    /// yet, so drawing would put a pointer somewhere it is not; or the host
    /// says the pointer is on another monitor.
    #[must_use]
    pub fn view(&self) -> Option<CursorView> {
        if self.tracking == Tracking::Silent || !self.on_screen {
            return None;
        }
        Some(CursorView {
            at: self.predicted,
            shape: self.shape.clone(),
            tracking: self.tracking,
        })
    }

    /// Moves the pointer by what the user just did, at once.
    ///
    /// The displacement is what the client asked the host for, divided by the
    /// streamed picture it was measured in. What the host does with it is the
    /// host's business, and the difference is `gain`.
    pub fn moved(&mut self, now: Instant, dx: f32, dy: f32) {
        let delta = Spot::new(dx, dy);
        if !delta.length().is_finite() {
            return;
        }
        self.predicted = self.predicted.plus(delta.scaled(self.gain)).onto_screen();
        // The raw displacement, not the scaled one: it is one half of every
        // later measurement of what the scale actually is.
        self.applied.push_back((now, delta));
        self.forget_before(now);
    }

    /// Puts the pointer where the user pointed, which a client with a real
    /// mouse over the video knows outright.
    pub fn placed(&mut self, now: Instant, at: Spot) {
        if !at.length().is_finite() {
            return;
        }
        self.predicted = at.onto_screen();
        // Nothing before an absolute placement is worth replaying: the
        // placement supersedes it, and so does it supersede the movement any
        // pending measurement was going to be taken over.
        self.applied.clear();
        self.last = None;
        self.forget_before(now);
    }

    /// Takes what the host said about the pointer.
    ///
    /// `round_trip` is the transport's own estimate. It is what says how stale
    /// the report is, and therefore which of this client's own movements have
    /// to be replayed on top of it.
    pub fn reported(&mut self, now: Instant, update: &CursorUpdate, round_trip: Duration) {
        let report = match update {
            CursorUpdate::Untracked => {
                self.tracking = Tracking::DeadReckoned;
                self.on_screen = true;
                return;
            }
            CursorUpdate::Tracked(report) => report,
        };
        self.tracking = Tracking::Reported;
        self.take_shape(report);

        let Some(at) = report.at else {
            self.on_screen = false;
            self.last = None;
            return;
        };
        self.on_screen = true;
        let taken_at = now.checked_sub(round_trip / 2).unwrap_or(now);
        let spot = Self::spot_of(report, at);
        self.learn(taken_at, spot, Self::clear_of_the_edges(report, at));
        self.predicted = self.reconcile(taken_at, spot);
        self.forget_before(now);
    }

    /// Measures what the host did with the movements it was sent between the
    /// previous report and this one.
    ///
    /// Only from a pair of reports that both left the pointer clear of the
    /// screen's edges: a pointer against an edge stops moving while the client
    /// keeps asking it to, and the ratio of the two is then a measurement of
    /// the edge rather than of the host.
    fn learn(&mut self, taken_at: Instant, spot: Spot, clear: bool) {
        let previous = self.last.replace((taken_at, spot, clear));
        let Some((since, was, was_clear)) = previous else {
            return;
        };
        if !clear || !was_clear {
            return;
        }
        let asked = self
            .applied
            .iter()
            .filter(|(when, _)| *when >= since && *when < taken_at)
            .fold(Spot::default(), |sum, (_, delta)| sum.plus(*delta))
            .length();
        if asked < GAIN_SAMPLE {
            return;
        }
        let ratio = spot.minus(was).length() / asked;
        if !GAIN_BAND.contains(&ratio) {
            return;
        }
        self.gain = if std::mem::replace(&mut self.measured, true) {
            self.gain + (ratio - self.gain) * GAIN_BLEND
        } else {
            ratio
        };
    }

    /// Whether a reported position is far enough inside the screen that the
    /// movement which produced it was not cut short by an edge.
    fn clear_of_the_edges(report: &CursorReport, at: CursorPoint) -> bool {
        let across = i32::from(report.screen.width.get());
        let down = i32::from(report.screen.height.get());
        at.x > 0 && at.y > 0 && at.x < across - 1 && at.y < down - 1
    }

    /// Where the pointer really is, once the report has been aged forward
    /// through everything this client did after it was taken.
    fn reconcile(&self, taken_at: Instant, reported: Spot) -> Spot {
        let replayed = self
            .applied
            .iter()
            .filter(|(when, _)| *when >= taken_at)
            // Held on the screen at every step, exactly as [`Self::moved`]
            // holds it, so that the two reckonings are the same arithmetic. A
            // replay that added the whole excursion up before clamping would
            // hand back the travel an edge had already eaten, and a pointer
            // pushed into an edge and pulled away from it would jump by the
            // overshoot.
            .fold(reported.onto_screen(), |spot, (_, delta)| {
                spot.plus(delta.scaled(self.gain)).onto_screen()
            });
        if replayed.distance(self.predicted) >= SNAP {
            replayed
        } else {
            self.predicted
                .plus(replayed.minus(self.predicted).scaled(BLEND))
        }
    }

    /// The reported hotspot, as a fraction of the screen it was measured in.
    fn spot_of(report: &CursorReport, at: CursorPoint) -> Spot {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a screen is far smaller than the integers a float counts exactly"
        )]
        Spot::new(
            at.x as f32 / f32::from(report.screen.width.get()),
            at.y as f32 / f32::from(report.screen.height.get()),
        )
    }

    /// Keeps the picture the host sent, in the units whatever draws it needs.
    ///
    /// A report with no picture leaves the last one standing: the host sends it
    /// in the update that changes it and in no other.
    fn take_shape(&mut self, report: &CursorReport) {
        let Some(image) = report.image.as_ref() else {
            return;
        };
        if image.pixels().is_none() {
            // A picture whose rows do not add up cannot be drawn, and it is the
            // one thing the encoding cannot rule out. The previous one stands.
            return;
        }
        let across = f32::from(report.screen.width.get());
        let down = f32::from(report.screen.height.get());
        #[expect(
            clippy::cast_precision_loss,
            reason = "a cursor is at most a few hundred pixels across"
        )]
        let shape = CursorShape {
            serial: self.shape.as_ref().map_or(0, |had| had.serial) + 1,
            width: f32::from(image.width.get()) / across,
            height: f32::from(image.height.get()) / down,
            hotspot_x: report.at.map_or(0.0, |at| at.hotspot_x as f32) / across,
            hotspot_y: report.at.map_or(0.0, |at| at.hotspot_y as f32) / down,
            image: Arc::new(image.clone()),
        };
        self.shape = Some(shape);
    }

    fn forget_before(&mut self, now: Instant) {
        let horizon = now.checked_sub(REPLAY_WINDOW).unwrap_or(now);
        while self
            .applied
            .front()
            .is_some_and(|(when, _)| *when < horizon)
        {
            self.applied.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use tachyx_proto::control::Size;

    use super::*;

    const RTT: Duration = Duration::from_millis(20);

    fn screen() -> Size {
        Size::new(1000, 1000).expect("a size")
    }

    fn arrow() -> CursorImage {
        CursorImage::new(
            NonZeroU16::new(50).expect("nonzero"),
            NonZeroU16::new(50).expect("nonzero"),
            vec![0x7f; 50 * 50 * 4],
        )
        .expect("rows that add up")
    }

    fn at(x: i32, y: i32) -> CursorPoint {
        CursorPoint {
            x,
            y,
            hotspot_x: 5,
            hotspot_y: 1,
        }
    }

    fn tracked(at: Option<CursorPoint>, image: Option<CursorImage>) -> CursorUpdate {
        CursorUpdate::Tracked(CursorReport {
            screen: screen(),
            at,
            image,
        })
    }

    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-4
    }

    #[test]
    fn nothing_is_drawn_before_the_host_has_said_anything() {
        let tracker = CursorTracker::new();
        assert!(
            tracker.view().is_none(),
            "a pointer drawn at a guess is worse than none"
        );
    }

    #[test]
    fn the_finger_moves_the_pointer_without_waiting_for_the_host() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(500, 500)), Some(arrow())), RTT);
        tracker.moved(now + Duration::from_millis(1), 0.1, -0.2);

        let view = tracker.view().expect("the host reported the pointer");
        assert!(close(view.at.x, 0.6), "{:?}", view.at);
        assert!(close(view.at.y, 0.3), "{:?}", view.at);
    }

    #[test]
    fn the_pointer_never_leaves_the_screen() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(500, 500)), None), RTT);
        for step in 1..40 {
            tracker.moved(now + Duration::from_millis(step), 0.1, 0.1);
        }
        let view = tracker.view().expect("reported");
        assert!(
            close(view.at.x, 1.0) && close(view.at.y, 1.0),
            "{:?}",
            view.at
        );
    }

    #[test]
    fn a_report_with_nothing_done_since_places_the_pointer_exactly() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(250, 750)), None), RTT);
        let view = tracker.view().expect("reported");
        assert!(
            close(view.at.x, 0.25) && close(view.at.y, 0.75),
            "{:?}",
            view.at
        );
    }

    #[test]
    fn a_steady_drag_does_not_rubber_band() {
        // The host is exactly where the client steered it, but every report
        // describes the position as of half a round trip ago. Replaying what
        // the client did since is what keeps the two in step; without it the
        // pointer is dragged backwards by one report's worth of travel, over
        // and over.
        let start = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(start, &tracked(Some(at(0, 500)), None), RTT);

        let step = 0.01;
        let mut host = 0.0f32;
        for tick in 1..=60u32 {
            let now = start + Duration::from_millis(u64::from(tick) * 16);
            tracker.moved(now, step, 0.0);
            // What the host had done by the time this report was taken: every
            // movement older than half a round trip.
            host += step;
            let seen = (host - step * 0.625).max(0.0);
            #[expect(clippy::cast_possible_truncation, reason = "a test fixture")]
            tracker.reported(
                now,
                &tracked(Some(at((seen * 1000.0) as i32, 500)), None),
                RTT,
            );
        }
        let view = tracker.view().expect("reported");
        assert!(
            (view.at.x - host).abs() < 0.02,
            "the pointer lagged the drag by {:.3} of the screen",
            host - view.at.x
        );
    }

    /// How far one unit of asked-for movement moves the drawn pointer, which is
    /// the only way the gain is observable from outside.
    ///
    /// Leftwards, so that a pointer pinned against the right edge still moves.
    fn gain_of(tracker: &mut CursorTracker, now: Instant) -> f32 {
        let before = tracker.view().expect("reported").at.x;
        tracker.moved(now, -0.01, 0.0);
        (before - tracker.view().expect("reported").at.x) / 0.01
    }

    /// Drags right for `ticks` frames, with the host moving `host_gain` of what
    /// it is asked for, and reports its position half a round trip late.
    ///
    /// Returns where the host's pointer ended up.
    fn drag(tracker: &mut CursorTracker, start: Instant, ticks: u32, host_gain: f32) -> f32 {
        const STEP: f32 = 0.006;
        let mut host = 0.2f32;
        tracker.reported(start, &tracked(Some(at(200, 500)), None), RTT);
        for tick in 1..=ticks {
            let now = start + Duration::from_millis(u64::from(tick) * 16);
            tracker.moved(now, STEP, 0.0);
            host += STEP * host_gain;
            // What the host had done by the moment the report was taken, which
            // is half a round trip before it arrived.
            let seen = host - STEP * host_gain * 0.625;
            #[expect(clippy::cast_possible_truncation, reason = "a test fixture")]
            tracker.reported(
                now,
                &tracked(Some(at((seen * 1000.0) as i32, 500)), None),
                RTT,
            );
        }
        host
    }

    #[test]
    fn the_hosts_own_scale_is_measured_rather_than_assumed() {
        // The host moves half of what it is asked for. Nothing announces that:
        // the stream is a halving of the capture, and the wire says only that
        // the host scales by something.
        let start = Instant::now();
        let mut tracker = CursorTracker::new();
        let host = drag(&mut tracker, start, 60, 0.5);
        let view = tracker.view().expect("reported");
        assert!(
            (view.at.x - host).abs() < 0.01,
            "the drawn pointer ran {:.3} of the screen away from the real one",
            view.at.x - host
        );
        assert!(
            (gain_of(&mut tracker, start + Duration::from_secs(1)) - 0.5).abs() < 0.05,
            "the measured scale should have converged on the host's"
        );
    }

    #[test]
    fn a_host_that_moves_further_than_it_is_asked_is_measured_too() {
        let start = Instant::now();
        let mut tracker = CursorTracker::new();
        let host = drag(&mut tracker, start, 60, 2.0);
        assert!((tracker.view().expect("reported").at.x - host).abs() < 0.01);
    }

    #[test]
    fn a_pointer_someone_else_moved_teaches_nothing() {
        let start = Instant::now();
        let mut tracker = CursorTracker::new();
        drag(&mut tracker, start, 40, 0.5);
        let learned = gain_of(&mut tracker, start + Duration::from_millis(700));

        // A jump no movement of this client's explains. Believing it would
        // record a scale of about a hundred and throw the pointer across the
        // screen on the next touch.
        let later = start + Duration::from_millis(800);
        tracker.moved(later, 0.006, 0.0);
        tracker.reported(later, &tracked(Some(at(950, 500)), None), RTT);
        let after = gain_of(&mut tracker, start + Duration::from_millis(900));
        assert!(
            (after - learned).abs() < 0.05,
            "the scale moved from {learned:.3} to {after:.3} on one wild report"
        );
    }

    #[test]
    fn an_edge_the_pointer_is_pinned_against_teaches_nothing() {
        let start = Instant::now();
        let mut tracker = CursorTracker::new();
        drag(&mut tracker, start, 40, 0.5);
        let learned = gain_of(&mut tracker, start + Duration::from_millis(700));

        // Pushed into the right edge: the client keeps asking and the host's
        // pointer stops. The ratio of the two is a measurement of the edge.
        let mut now = start + Duration::from_millis(800);
        for _ in 0..30 {
            now += Duration::from_millis(16);
            tracker.moved(now, 0.02, 0.0);
            tracker.reported(now, &tracked(Some(at(999, 500)), None), RTT);
        }
        let after = gain_of(&mut tracker, now + Duration::from_millis(16));
        assert!(
            (after - learned).abs() < 0.05,
            "the scale moved from {learned:.3} to {after:.3} against an edge"
        );
    }

    #[test]
    fn a_pointer_pushed_into_an_edge_does_not_jump_when_it_comes_back() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(950, 500)), None), RTT);

        // Shoved off the right edge and pulled back. The client's own reckoning
        // stopped at the edge on the way out, so the overshoot is gone.
        let later = now + Duration::from_millis(1);
        tracker.moved(later, 0.2, 0.0);
        tracker.moved(later, -0.2, 0.0);
        assert!(close(tracker.view().expect("reported").at.x, 0.8));

        // The same report again: nothing has happened on the host that the
        // client did not already know. A replay that added the excursion up
        // before clamping would give the overshoot back and jump the pointer a
        // fifth of the screen to the right.
        tracker.reported(later, &tracked(Some(at(950, 500)), None), RTT);
        let view = tracker.view().expect("reported");
        assert!(
            close(view.at.x, 0.8),
            "the pointer jumped to {:?} coming off the edge",
            view.at
        );
    }

    #[test]
    fn a_pointer_moved_by_someone_else_is_snapped_to() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(100, 100)), None), RTT);
        // Nothing the client did explains this: the user moved the host's own
        // mouse. Easing towards it would drag a ghost across the screen.
        tracker.reported(
            now + Duration::from_millis(16),
            &tracked(Some(at(900, 900)), None),
            RTT,
        );
        let view = tracker.view().expect("reported");
        assert!(
            close(view.at.x, 0.9) && close(view.at.y, 0.9),
            "{:?}",
            view.at
        );
    }

    #[test]
    fn a_small_disagreement_is_eased_away_rather_than_jumped() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(500, 500)), None), RTT);
        tracker.reported(
            now + Duration::from_millis(16),
            &tracked(Some(at(520, 500)), None),
            RTT,
        );
        let view = tracker.view().expect("reported");
        assert!(
            view.at.x > 0.5 && view.at.x < 0.52,
            "a jump of every rounding error would jitter: {:?}",
            view.at
        );
    }

    #[test]
    fn a_pointer_on_another_monitor_is_not_drawn() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(500, 500)), Some(arrow())), RTT);
        assert!(tracker.view().is_some());
        tracker.reported(now, &tracked(None, None), RTT);
        assert!(
            tracker.view().is_none(),
            "a pointer that left the captured output must not be drawn on it"
        );
    }

    #[test]
    fn the_picture_stands_until_one_that_replaces_it_arrives() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(500, 500)), Some(arrow())), RTT);
        let first = tracker.view().expect("reported").shape.expect("a picture");
        assert!(close(first.width, 0.05) && close(first.height, 0.05));
        assert!(close(first.hotspot_x, 0.005) && close(first.hotspot_y, 0.001));

        tracker.reported(now, &tracked(Some(at(510, 500)), None), RTT);
        let carried = tracker.view().expect("reported").shape.expect("a picture");
        assert_eq!(
            carried.serial, first.serial,
            "an update that carries no picture must not look like a new one"
        );

        let beam = CursorImage::new(
            NonZeroU16::new(20).expect("nonzero"),
            NonZeroU16::new(40).expect("nonzero"),
            vec![0x11; 20 * 40 * 4],
        )
        .expect("rows that add up");
        tracker.reported(now, &tracked(Some(at(520, 500)), Some(beam)), RTT);
        let changed = tracker.view().expect("reported").shape.expect("a picture");
        assert_eq!(changed.serial, first.serial + 1);
        assert!(close(changed.width, 0.02) && close(changed.height, 0.04));
    }

    #[test]
    fn a_host_that_cannot_track_the_pointer_still_gets_one_drawn() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &CursorUpdate::Untracked, RTT);
        let view = tracker.view().expect("a pointer that drifts beats none");
        assert_eq!(view.tracking, Tracking::DeadReckoned);
        assert!(view.shape.is_none(), "the client draws its own");
        assert!(close(view.at.x, 0.5) && close(view.at.y, 0.5));

        tracker.moved(now, 0.25, 0.0);
        let view = tracker.view().expect("still drawn");
        assert!(close(view.at.x, 0.75), "{:?}", view.at);
    }

    #[test]
    fn a_mouse_over_the_video_places_the_pointer_outright() {
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.reported(now, &tracked(Some(at(0, 0)), None), RTT);
        tracker.placed(now, Spot::new(0.8, 0.2));
        let view = tracker.view().expect("reported");
        assert!(
            close(view.at.x, 0.8) && close(view.at.y, 0.2),
            "{:?}",
            view.at
        );
    }

    #[test]
    fn a_picture_whose_rows_do_not_add_up_never_reaches_the_screen() {
        #[derive(serde::Serialize)]
        struct Claimed {
            width: u16,
            height: u16,
            pixels: Vec<u8>,
        }

        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        let bytes = postcard::to_stdvec(&Claimed {
            width: 50,
            height: 50,
            pixels: vec![0; 8],
        })
        .expect("encodes");
        let lie: CursorImage = postcard::from_bytes(&bytes).expect("decodes");
        tracker.reported(now, &tracked(Some(at(500, 500)), Some(lie)), RTT);
        assert!(
            tracker.view().expect("reported").shape.is_none(),
            "drawing it would read past the end of what arrived"
        );
    }
}
