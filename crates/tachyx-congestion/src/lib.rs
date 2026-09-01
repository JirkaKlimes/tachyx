//! What the host is allowed to spend, and how it finds out.
//!
//! quinn exposes no per-datagram acknowledgement to the application, and its
//! own bandwidth estimate was measured reporting 0.00 Mbit/s on a healthy 100
//! Mbit/s path for every sample of a twenty-second run: its sampler discards
//! any measurement taken while the sender is limited by its own supply, and a
//! sender producing sixty frames a second is in that state whenever an
//! acknowledgement arrives. So the only information this system can have about
//! the path is what the receiver reports, and this crate is what reads it.
//!
//! Four parts, and the design behind each is in `spec/congestion.md`:
//!
//! - [`delay`] groups the arrivals and says whether a queue is filling.
//! - [`rate`] turns that into a rate, always against what the path has already
//!   carried rather than against what the host meant to send.
//! - [`loss`] runs beside it on the cumulative counters, for the link that
//!   drops packets without ever building a queue.
//! - [`probe`] tests the link with a cluster released above the current rate,
//!   because multiplicative increase alone takes 20.9 seconds to recover a
//!   factor of five and probing takes a quarter of a second.

mod delay;
mod history;
mod loss;
mod probe;
mod rate;

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use tachyx_proto::control::Feedback;

pub use crate::delay::Signal;
pub use crate::history::{ClusterId, Release};
pub use crate::loss::Losses;
pub use crate::probe::{Probe, cluster_datagrams};

use crate::history::History;
use crate::loss::LossControl;
use crate::probe::Probes;
use crate::rate::{Acknowledged, Aimd};

/// The least the video path is ever targeted at, in bits a second.
///
/// Not arbitrary: the smallest rung of the ladder, 860x360 at 15 frames a
/// second, needs 279 kbit/s at the quality figure the ladder opens on. Under
/// this there is no picture worth sending, so the controller holds here and
/// lets loss speak rather than chasing a number that cannot be spent.
pub const MIN_TARGET: u32 = 300_000;

/// What the estimate reserves for everything that is not video.
///
/// Flat rather than a percentage, because control's cost does not scale with
/// the video rate: 35 kbit/s of receiver reports at the highest rate this runs
/// at, 2 kbit/s of heartbeat, and up to 100 kbit/s of input, which at a
/// sustained drag is the dominant term. That is 7.5% of a 2 Mbit/s link and
/// 0.75% of a 20 Mbit/s one, and both are the right answer.
pub const CONTROL_RESERVE: u32 = 150_000;

/// Where a session starts, before anything has been measured.
///
/// Not because 2 Mbit/s is a good guess about the link, but because it is a
/// rate essentially every link this runs on carries, and because a low start is
/// cheap when probing makes it brief. libwebrtc starts at 300 kbit/s for the
/// same reason; a remote desktop can afford more because it can afford to be
/// wrong for a quarter of a second and cannot afford an unreadable first
/// picture.
pub const START_TARGET: u32 = 2_000_000;

/// What one report, or one tick, changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Update {
    /// Bits a second the video path should now spend.
    pub video: NonZeroU32,
    /// The whole estimate, video and control together.
    pub estimate: NonZeroU32,
    /// Loss over the interval the report covers. Zero on a tick.
    pub losses: Losses,
    /// A cluster to release, if a probe is due.
    pub probe: Option<Probe>,
}

/// The host's estimate of what the path will carry.
#[derive(Debug)]
pub struct Controller {
    history: History,
    delay: delay::Delay,
    acknowledged: Acknowledged,
    aimd: Aimd,
    loss: LossControl,
    probes: Probes,
    ceiling: f64,
    estimate: f64,
    signal: Signal,
}

impl Controller {
    /// Starts a controller for a session that may spend up to `ceiling` bits a
    /// second.
    #[must_use]
    pub fn new(ceiling: NonZeroU32, now: Instant) -> Self {
        let ceiling = f64::from(ceiling.get()).max(f64::from(MIN_TARGET));
        let start = f64::from(START_TARGET).min(ceiling);
        Self {
            history: History::new(now),
            delay: delay::Delay::default(),
            acknowledged: Acknowledged::default(),
            aimd: Aimd::new(start, f64::from(MIN_TARGET), ceiling),
            loss: LossControl::new(start, f64::from(MIN_TARGET), ceiling),
            probes: Probes::new(ceiling),
            ceiling,
            estimate: start,
            signal: Signal::Normal,
        }
    }

    /// Records one datagram the transport released.
    ///
    /// A report can only say anything about a datagram this was told about, and
    /// a datagram whose record has aged out is skipped rather than guessed at.
    pub fn released(&mut self, release: &Release) {
        self.history.push(release);
    }

    /// Raises or lowers what the session is allowed to ask for.
    pub fn set_ceiling(&mut self, ceiling: NonZeroU32) {
        self.ceiling = f64::from(ceiling.get()).max(f64::from(MIN_TARGET));
        self.aimd.set_ceiling(self.ceiling);
        self.loss.set_ceiling(self.ceiling);
        self.probes.set_ceiling(self.ceiling);
    }

    /// Tells the rate control what the path's round trip is, which is what
    /// paces the additive increase.
    pub const fn set_rtt(&mut self, rtt: Duration) {
        self.aimd.set_rtt(rtt);
    }

    /// The estimate, video and control together, in bits a second.
    #[must_use]
    pub fn estimate(&self) -> NonZeroU32 {
        as_rate(self.estimate)
    }

    /// What the video path should spend, which is the estimate less the flat
    /// reserve every other message on the connection shares.
    #[must_use]
    pub fn video_target(&self) -> NonZeroU32 {
        as_rate((self.estimate - f64::from(CONTROL_RESERVE)).max(f64::from(MIN_TARGET)))
    }

    /// What the path last looked like.
    #[must_use]
    pub const fn signal(&self) -> Signal {
        self.signal
    }

    /// Takes one receiver report.
    pub fn feedback(&mut self, report: &Feedback, now: Instant) -> Update {
        let at = self.history.micros(now);
        let Some(packets) = self.history.read(report) else {
            // The report names a sequence this host cannot place against what
            // it sent. Sixteen bits wrap every 93 seconds at 20 Mbit/s, and the
            // unwrapping is only correct while reports arrive more often than
            // half a wrap, so the record is discarded rather than trusted.
            self.history.clear();
            return self.update(Losses::default(), None);
        };

        for packet in &packets {
            self.signal = self.delay.observe(packet.sent, packet.arrived);
            self.acknowledged
                .observe(packet.arrived, u64::from(packet.wire));
        }

        if let Some(measured) = self.probes.read(&packets, at) {
            // A probe sets the rate outright instead of waiting for the
            // increase to walk there. That is the whole reason it exists.
            self.aimd.probed(measured, at);
            self.loss.probed(measured);
        }

        let limit = self.aimd.observe(self.signal, self.acknowledged.rate(), at);
        let losses = self.loss.observe(report.lost, report.received, at);
        self.loss.lower_to(limit);
        self.estimate = self.loss.rate();
        self.update(losses, Some(at))
    }

    /// Looks at the link without a report to read.
    ///
    /// A still desktop produces no frames, so the client has nothing to report
    /// the arrival of and the controller would otherwise never hear that the
    /// link recovered. This is what schedules the padding probe that breaks
    /// that silence.
    pub fn tick(&mut self, now: Instant) -> Update {
        let at = self.history.micros(now);
        self.update(Losses::default(), Some(at))
    }

    fn update(&mut self, losses: Losses, at: Option<i64>) -> Update {
        let probe = at.and_then(|at| self.probes.due(self.estimate, self.signal, at));
        Update {
            video: self.video_target(),
            estimate: self.estimate(),
            losses,
            probe,
        }
    }
}

/// A rate as the rest of the system takes it, saturating rather than wrapping.
fn as_rate(bits: f64) -> NonZeroU32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into the range immediately before the cast"
    )]
    let bits = bits.clamp(1.0, f64::from(u32::MAX)) as u32;
    NonZeroU32::new(bits).unwrap_or(NonZeroU32::MIN)
}
