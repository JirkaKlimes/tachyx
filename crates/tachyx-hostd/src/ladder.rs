//! Choosing what to ask the encoder for.
//!
//! The transport reports what the link will carry. This turns that into an
//! encode size, a frame rate and a bitrate.
//!
//! Desktop content resists downscaling badly. Sharp text degrades far worse
//! under a resample than under coarser quantisation, and a desktop is mostly
//! static, so a low bitrate is cheap where a low resolution is not. The ladder
//! therefore spends bitrate first and reaches for resolution last.

use std::time::{Duration, Instant};

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

/// Bits per pixel per frame, the measure the ladder decides on.
///
/// Resolution and frame rate both cancel, so one pair of thresholds covers every
/// rung instead of a table of bitrates per size.
fn bits_per_pixel_per_frame(bits_per_second: u32, size: Size, fps: u16) -> f32 {
    let pixel_rate = size.pixels().saturating_mul(u32::from(fps));
    if pixel_rate == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "both values are far below the exact integer range of a float"
    )]
    let ratio = bits_per_second as f32 / pixel_rate as f32;
    ratio
}

/// Below this the picture is visibly starved and the rung is too tall.
///
/// Full-motion streaming at 1080p60 sits between 0.08 and 0.16 in practice, so
/// this is already well under what a video service would accept, which is the
/// latitude desktop content earns.
const STARVED: f32 = 0.045;

/// Above this there is room to climb.
///
/// The gap to [`STARVED`] is the hysteresis: a rung must be comfortably affordable
/// before the ladder takes it, or it oscillates across a single threshold.
const COMFORTABLE: f32 = 0.09;

/// How long a reading must persist before the ladder steps down.
const FALL_DWELL: Duration = Duration::from_secs(2);

/// How long a reading must persist before the ladder steps up.
///
/// Climbing is slower than falling on purpose. A step costs a key frame, and
/// being one rung low is far cheaper than flapping between two.
const RISE_DWELL: Duration = Duration::from_secs(5);

/// The smallest gap between resolution changes.
///
/// Each one costs a key frame and makes the client reconfigure its decoder.
const RESOLUTION_DWELL: Duration = Duration::from_secs(5);

/// Which way the ladder wants to move, and since when.
#[derive(Debug, Clone, Copy)]
enum Pressure {
    Settled,
    Falling(Instant),
    Rising(Instant),
}

/// Tracks the encode settings a session should be using.
#[derive(Debug)]
pub struct Ladder {
    rungs: Vec<Size>,
    rung: usize,
    fps: u16,
    bitrate: u32,
    pressure: Pressure,
    last_resolution_change: Option<Instant>,
}

/// A change the ladder wants applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The size to encode at.
    pub size: Size,
    /// The frame rate to declare.
    ///
    /// The encoder budgets bits as the bitrate divided by this, and ignores when
    /// frames actually arrive, so it is part of the same decision as the bitrate
    /// rather than an independent knob.
    pub fps: u16,
    /// The target, in bits per second.
    pub bitrate: u32,
}

impl Ladder {
    /// Builds a ladder of sizes no larger than `capture`, preserving its aspect
    /// ratio so the scaler is never asked to stretch.
    ///
    /// The scaler does not preserve aspect ratio unless told to, and the
    /// correction it records never reaches the bitstream, so a client cannot
    /// recover the geometry. Choosing rungs that keep the ratio avoids the
    /// question entirely.
    #[must_use]
    pub fn new(capture: Size, fps: u16, bitrate: u32) -> Self {
        // Halvings, and nothing between them. Measured on this hardware, scaling
        // to three quarters costs more than not scaling at all: 9.8 milliseconds
        // against 7.6 at full size, while a halving costs 5.1. A fraction that
        // is not a power of two puts the scaler on an expensive filter path, so
        // an intermediate rung would buy a smaller picture for more latency.
        //
        // Few rungs is not a compromise here. Resolution is the last thing this
        // ladder spends, after bitrate, so the fine-grained work happens before
        // any of these are reached.
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
        let rung = rungs.len() - 1;
        Self {
            rungs,
            rung,
            fps,
            bitrate,
            pressure: Pressure::Settled,
            last_resolution_change: None,
        }
    }

    /// The settings currently in force.
    #[must_use]
    pub fn settings(&self) -> Settings {
        Settings {
            size: self.rungs[self.rung],
            fps: self.fps,
            bitrate: self.bitrate,
        }
    }

    /// Feeds the ladder a fresh estimate of what the link will carry.
    ///
    /// Returns the settings to apply when they differ from what is in force.
    /// The bitrate follows the estimate immediately, because it is the cheap
    /// knob; the resolution follows only after the pressure has persisted and
    /// the dwell has elapsed.
    pub fn observe(&mut self, available_bits: u32, now: Instant) -> Option<Settings> {
        let before = self.settings();
        self.bitrate = available_bits;

        let ratio = bits_per_pixel_per_frame(self.bitrate, self.rungs[self.rung], self.fps);
        self.pressure = match (ratio, self.pressure) {
            (r, Pressure::Falling(since)) if r < STARVED => Pressure::Falling(since),
            (r, _) if r < STARVED => Pressure::Falling(now),
            (r, Pressure::Rising(since)) if r > COMFORTABLE => Pressure::Rising(since),
            (r, _) if r > COMFORTABLE => Pressure::Rising(now),
            _ => Pressure::Settled,
        };

        let rested = self
            .last_resolution_change
            .is_none_or(|last| now.duration_since(last) >= RESOLUTION_DWELL);
        if rested {
            match self.pressure {
                Pressure::Falling(since)
                    if now.duration_since(since) >= FALL_DWELL && self.rung > 0 =>
                {
                    self.rung -= 1;
                    self.settle(now);
                }
                Pressure::Rising(since)
                    if now.duration_since(since) >= RISE_DWELL
                        && self.rung + 1 < self.rungs.len() =>
                {
                    self.rung += 1;
                    self.settle(now);
                }
                _ => {}
            }
        }

        let after = self.settings();
        (after != before).then_some(after)
    }

    fn settle(&mut self, now: Instant) {
        self.pressure = Pressure::Settled;
        self.last_resolution_change = Some(now);
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

    const NATIVE: Size = Size {
        width: 2880,
        height: 1800,
    };

    fn ladder() -> Ladder {
        Ladder::new(NATIVE, 60, 40_000_000)
    }

    #[test]
    fn every_rung_is_even_in_both_axes() {
        for rung in ladder().rungs {
            assert_eq!(rung.width % 2, 0, "{rung:?}");
            assert_eq!(rung.height % 2, 0, "{rung:?}");
        }
    }

    #[test]
    fn the_capture_size_is_offered_even_when_it_is_not_a_multiple_of_sixteen() {
        // The encoder codes 1800 as 1808 and attaches a crop, so the size a
        // client is told to display and the size actually coded are reported
        // separately rather than the ladder refusing the size people asked for.
        assert_ne!(NATIVE.height % 16, 0);
        assert!(ladder().rungs.contains(&NATIVE));
    }

    #[test]
    fn the_rungs_are_halvings_and_nothing_between() {
        // Measured: scaling to three quarters costs more than not scaling at
        // all, so an intermediate rung would buy a smaller picture for more
        // latency. Any rung that is not a power-of-two fraction is a bug.
        let ladder = ladder();
        for rung in &ladder.rungs {
            let ratio = f64::from(NATIVE.width) / f64::from(rung.width);
            assert!(
                (ratio - ratio.round()).abs() < 0.01 && ratio.round().log2().fract() == 0.0,
                "{rung:?} is not a halving of {NATIVE:?}"
            );
        }
    }

    #[test]
    fn the_ladder_starts_at_the_capture_size() {
        assert_eq!(ladder().settings().size, NATIVE);
    }

    #[test]
    fn every_rung_keeps_the_capture_aspect_ratio() {
        let native = f64::from(NATIVE.width) / f64::from(NATIVE.height);
        for rung in ladder().rungs {
            let ratio = f64::from(rung.width) / f64::from(rung.height);
            assert!(
                (ratio - native).abs() < 0.02,
                "{rung:?} would make the scaler stretch the picture"
            );
        }
    }

    #[test]
    fn bitrate_follows_the_estimate_without_waiting() {
        let mut ladder = ladder();
        let now = Instant::now();
        let applied = ladder
            .observe(30_000_000, now)
            .expect("a bitrate change applies at once");
        assert_eq!(applied.bitrate, 30_000_000);
        assert_eq!(applied.size, NATIVE, "resolution must not move this fast");
    }

    #[test]
    fn a_starved_link_steps_the_resolution_down_but_only_after_the_dwell() {
        let mut ladder = ladder();
        let start = Instant::now();
        ladder.observe(2_000_000, start);
        assert_eq!(ladder.settings().size, NATIVE, "the dwell has not elapsed");

        let settled = ladder
            .observe(2_000_000, start + FALL_DWELL)
            .expect("a sustained shortfall steps down");
        assert!(
            settled.size.pixels() < NATIVE.pixels(),
            "expected a smaller rung, got {settled:?}"
        );
    }

    #[test]
    fn a_brief_dip_does_not_move_the_resolution() {
        let mut ladder = ladder();
        let start = Instant::now();
        ladder.observe(2_000_000, start);
        ladder.observe(40_000_000, start + Duration::from_millis(500));
        ladder.observe(40_000_000, start + FALL_DWELL + Duration::from_secs(1));
        assert_eq!(
            ladder.settings().size,
            NATIVE,
            "a transient dip must not cost a key frame"
        );
    }

    #[test]
    fn the_ladder_cannot_oscillate_between_two_rungs() {
        let mut ladder = ladder();
        let mut now = Instant::now();
        for _ in 0..40 {
            now += Duration::from_millis(500);
            ladder.observe(2_000_000, now);
        }
        let bottom = ladder.settings().size;

        let mut changes = 0;
        for _ in 0..40 {
            now += Duration::from_millis(500);
            // A reading that sits exactly between the two thresholds.
            let between = bits_per_pixel_per_frame(1, bottom, 60);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a contrived bitrate for one assertion"
            )]
            let bits = (f32::midpoint(STARVED, COMFORTABLE) / between) as u32;
            if ladder.observe(bits, now).is_some_and(|s| s.size != bottom) {
                changes += 1;
            }
        }
        assert_eq!(
            changes, 0,
            "a reading inside the hysteresis band must settle"
        );
    }

    #[test]
    fn recovery_climbs_back_up() {
        let mut ladder = ladder();
        let mut now = Instant::now();
        for _ in 0..20 {
            now += Duration::from_secs(1);
            ladder.observe(1_000_000, now);
        }
        let bottom = ladder.settings().size;
        assert!(bottom.pixels() < NATIVE.pixels());

        for _ in 0..40 {
            now += Duration::from_secs(1);
            ladder.observe(60_000_000, now);
        }
        assert!(
            ladder.settings().size.pixels() > bottom.pixels(),
            "a link that recovered must be used"
        );
    }
}
