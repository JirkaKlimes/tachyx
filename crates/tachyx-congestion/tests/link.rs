//! The controller against a shaped link, and against the reference.
//!
//! The link is a fluid drop-tail bottleneck with a one-way delay: a datagram
//! offered at `now` goes on the wire at `max(next_free, now)` and occupies
//! `bytes * 8 / rate` of it, so the backlog is `next_free - now` and a datagram
//! offered when that already exceeds the queue's depth is dropped. That is what
//! a per-station queue inside an access point does, and it is the shape the
//! measurements in `spec/findings.md` were taken against.
//!
//! The same trace drives `goog_cc::GoogCcNetworkController`, which is a
//! direct port of libwebrtc's `GoogCC`. It is a development dependency and
//! nothing ships: every algorithm module in that crate is private, so it can be
//! driven but not borrowed from, and what it is worth is exactly this: an
//! answer to hold ours against.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use goog_cc::network_control::{NetworkControllerConfig, NetworkControllerInterface};
use goog_cc::transport::{
    NetworkAvailability, PacedPacketInfo, PacketResult, ProcessInterval, SentPacket,
    TargetRateConstraints, TransportPacketsFeedback,
};
use goog_cc::units::{DataRate, DataSize, Timestamp};
use goog_cc::{GoogCcConfig, GoogCcNetworkController};
use tachyx_congestion::{
    CONTROL_RESERVE, Controller, MIN_TARGET, Release, START_TARGET, cluster_datagrams,
};
use tachyx_proto::control::{FEEDBACK_INTERVAL, Feedback, PacketArrival};
use tachyx_proto::media::Seq;

/// Bytes one video datagram weighs on the wire.
const DATAGRAM: usize = 1434;

/// The simulation's own clock granularity.
const STEP: i64 = 500;

/// The pacing factor the transport releases at, which is libwebrtc's.
const PACE_PERCENT: i64 = 250;

/// The frame interval the sender produces at.
const FRAME: i64 = 16_667;

/// What the session is allowed to ask for.
const CEILING: u32 = 20_000_000;

/// A one-way delay of half the 13.91 ms round trip measured to the phone.
const DELAY: i64 = 6_955;

/// Bytes the bottleneck holds before it drops the tail. 128 kB is the depth at
/// which a burst of the measured key frame stopped being destroyed.
const QUEUE: i64 = 128_000;

/// A path whose capacity changes under the sender.
struct Link {
    /// Bits a second, and when each figure takes effect.
    schedule: Vec<(i64, u64)>,
    next_free: i64,
}

impl Link {
    fn new(schedule: &[(i64, u64)]) -> Self {
        Self {
            schedule: schedule.to_vec(),
            next_free: 0,
        }
    }

    fn rate(&self, now: i64) -> u64 {
        self.schedule
            .iter()
            .filter(|(at, _)| *at <= now)
            .map(|(_, rate)| *rate)
            .next_back()
            .unwrap_or(10_000_000)
    }

    /// Offers one datagram, returning when it arrives or `None` if the queue
    /// dropped it.
    fn offer(&mut self, bytes: usize, now: i64) -> Option<i64> {
        let rate = self.rate(now);
        let start = self.next_free.max(now);
        let backlog = (start - now) * i64::try_from(rate).unwrap_or(i64::MAX) / 8_000_000;
        if backlog + i64::try_from(bytes).unwrap_or(0) > QUEUE {
            return None;
        }
        let serialise =
            i64::try_from(bytes).unwrap_or(0) * 8_000_000 / i64::try_from(rate).unwrap_or(1);
        self.next_free = start + serialise;
        Some(self.next_free + DELAY)
    }
}

/// What the receiver knows, and the report it writes from it.
#[derive(Default)]
struct Receiver {
    arrivals: BTreeMap<u64, i64>,
    highest: Option<u64>,
    next: u64,
    received: u64,
    lost: u64,
}

impl Receiver {
    fn arrived(&mut self, ext: u64, at: i64) {
        self.arrivals.insert(ext, at);
        let highest = self.highest.get_or_insert(ext);
        *highest = (*highest).max(ext);
    }

    /// The report the client would write at `now`, and the extended numbers it
    /// covers.
    fn report(&mut self, now: i64) -> Option<(Feedback, Vec<u64>)> {
        let highest = self
            .arrivals
            .range(..)
            .filter(|(_, at)| **at <= now)
            .map(|(ext, _)| *ext)
            .next_back()?;
        if highest < self.next {
            return None;
        }
        let covered: Vec<u64> = (self.next..=highest).collect();
        let mut packets = Vec::with_capacity(covered.len());
        let mut reference = None;
        let mut previous = None;
        for ext in &covered {
            if let Some(at) = self.arrivals.get(ext).filter(|at| **at <= now) {
                self.received += 1;
                let ticks = at / 250;
                let delta = previous.map_or_else(
                    || {
                        reference = Some(ticks);
                        0
                    },
                    |held| ticks - held,
                );
                previous = Some(ticks);
                packets.push(PacketArrival::Received(
                    i16::try_from(delta).expect("a report interval fits a signed tick count"),
                ));
            } else {
                self.lost += 1;
                packets.push(PacketArrival::Missing);
            }
        }
        self.next = highest + 1;
        let base = Seq(u16::try_from(covered[0] % 65_536).expect("masked"));
        Some((
            Feedback {
                base,
                reference: u32::try_from(reference.unwrap_or(now / 250)).unwrap_or(u32::MAX),
                packets,
                lost: self.lost,
                received: self.received,
            },
            covered,
        ))
    }
}

/// A probe the controller asked for.
#[derive(Debug, Clone, Copy)]
struct Armed {
    id: u32,
    rate: u32,
    left: usize,
    probe: PacedPacketInfo,
}

/// Which controller is driving.
enum Driver {
    Ours(Box<Controller>),
    /// The reference, and the target it last reported.
    Oracle(Box<GoogCcNetworkController>, u32),
}

impl Driver {
    fn ours(now: Instant) -> Self {
        Self::Ours(Box::new(Controller::new(
            NonZeroU32::new(CEILING).unwrap(),
            now,
        )))
    }

    fn oracle() -> Self {
        let start = Timestamp::from_micros(0);
        let mut controller = GoogCcNetworkController::new(
            NetworkControllerConfig {
                constraints: TargetRateConstraints {
                    at_time: start,
                    min_data_rate: Some(DataRate::from_bits_per_sec(i64::from(MIN_TARGET))),
                    max_data_rate: Some(DataRate::from_bits_per_sec(i64::from(CEILING))),
                    starting_rate: Some(DataRate::from_bits_per_sec(i64::from(START_TARGET))),
                },
                ..Default::default()
            },
            // The trace carries no RTCP, so loss and round trip have to be
            // taken from the feedback itself.
            GoogCcConfig {
                feedback_only: true,
            },
        );
        controller.on_network_availability(NetworkAvailability {
            at_time: start,
            network_available: true,
        });
        Self::Oracle(Box::new(controller), START_TARGET)
    }

    /// Bits a second the video path may spend.
    fn target(&self) -> u32 {
        match self {
            Self::Ours(controller) => controller.video_target().get(),
            Self::Oracle(_, target) => target.saturating_sub(CONTROL_RESERVE).max(MIN_TARGET),
        }
    }
}

/// What one run produced.
struct Trace {
    /// The video target, sampled every 100 ms.
    samples: Vec<(i64, u32)>,
    delivered: u64,
    dropped: u64,
}

impl Trace {
    /// The target in force at `at` microseconds.
    fn at(&self, at: i64) -> u32 {
        self.samples
            .iter()
            .filter(|(when, _)| *when <= at)
            .map(|(_, rate)| *rate)
            .next_back()
            .unwrap_or(START_TARGET)
    }

    /// The first moment after `from` at which the target satisfies `wanted`.
    fn first(&self, from: i64, wanted: impl Fn(u32) -> bool) -> Option<i64> {
        self.samples
            .iter()
            .find(|(when, rate)| *when >= from && wanted(*rate))
            .map(|(when, _)| *when)
    }
}

/// Runs `driver` against `link` for `seconds`, delivering every report.
fn simulate(schedule: &[(i64, u64)], seconds: i64, driver: Driver) -> Trace {
    run(schedule, seconds, driver, None)
}

/// Runs `driver` against `link` for `seconds`, with the return path dead over
/// `outage` if one is given.
///
/// An outage drops the reports the client writes rather than the datagrams the
/// host sends, because that is what a starved uplink does: the downlink still
/// carries video, and the feedback that would answer it never arrives.
#[expect(
    clippy::too_many_lines,
    reason = "a simulation is one procedure; splitting it would hide the schedule"
)]
fn run(
    schedule: &[(i64, u64)],
    seconds: i64,
    mut driver: Driver,
    outage: Option<(i64, i64)>,
) -> Trace {
    let epoch = Instant::now();
    let mut link = Link::new(schedule);
    let mut receiver = Receiver::default();
    let mut oracle_sent: BTreeMap<u64, SentPacket> = BTreeMap::new();
    let mut samples = Vec::new();

    let mut ext = 0u64;
    let mut pending = 0usize;
    let mut paid = 0i64;
    let mut armed: Option<Armed> = None;
    let mut next_frame = 0i64;
    let mut next_report = i64::try_from(FEEDBACK_INTERVAL.as_micros()).unwrap();
    let mut next_process = 25_000i64;
    let mut next_tick = 250_000i64;
    let mut delivered = 0u64;
    let mut dropped = 0u64;
    let mut reports: Vec<(i64, Feedback, Vec<u64>)> = Vec::new();

    for now in (0..seconds * 1_000_000).step_by(usize::try_from(STEP).unwrap()) {
        let target = driver.target();
        if now >= next_frame {
            next_frame += FRAME;
            pending +=
                usize::try_from(u64::from(target) * FRAME.unsigned_abs() / 8_000_000).unwrap_or(0);
        }

        // Release at the pacing rate, or at the probe's rate while a cluster is
        // going out. A cluster with no media behind it is padding, which is the
        // whole reason padding exists.
        loop {
            let rate = armed.map_or(i64::from(target) * PACE_PERCENT / 100, |probe| {
                i64::from(probe.rate)
            });
            if paid > now || (pending < DATAGRAM && armed.is_none()) {
                break;
            }
            let bytes = DATAGRAM;
            pending = pending.saturating_sub(bytes);
            paid = paid.max(now) + i64::try_from(bytes).unwrap() * 8_000_000 / rate.max(1);

            let cluster = armed.map(|probe| probe.id);
            let probe = armed.map_or_else(PacedPacketInfo::default, |probe| probe.probe);
            match &mut driver {
                Driver::Ours(controller) => controller.released(&tachyx_congestion::Release {
                    seq: Seq(u16::try_from(ext % 65_536).expect("masked")),
                    at: epoch + Duration::from_micros(now.unsigned_abs()),
                    wire: u16::try_from(bytes).unwrap(),
                    cluster: cluster.map(tachyx_congestion::ClusterId),
                }),
                Driver::Oracle(controller, _) => {
                    let packet = SentPacket {
                        send_time: Timestamp::from_micros(now),
                        size: DataSize::from_bytes(i64::try_from(bytes).unwrap()),
                        pacing_info: probe,
                        sequence_number: i64::try_from(ext).unwrap(),
                        ..Default::default()
                    };
                    oracle_sent.insert(ext, packet);
                    controller.on_sent_packet(packet);
                }
            }

            if let Some(at) = link.offer(bytes, now) {
                receiver.arrived(ext, at);
                delivered += 1;
            } else {
                dropped += 1;
            }
            ext += 1;
            if let Some(probe) = &mut armed {
                probe.left -= 1;
                if probe.left == 0 {
                    armed = None;
                }
            }
        }

        if now >= next_report {
            next_report += i64::try_from(FEEDBACK_INTERVAL.as_micros()).unwrap();
            if let Some((report, covered)) = receiver.report(now) {
                // The report travels the return path before the host reads it,
                // and never arrives while that path is dead.
                let dead = outage.is_some_and(|(from, to)| (from..to).contains(&now));
                if !dead {
                    reports.push((now + DELAY, report, covered));
                }
            }
        }

        while reports.first().is_some_and(|(at, _, _)| *at <= now) {
            let (at, report, covered) = reports.remove(0);
            armed = armed.or_else(|| match &mut driver {
                Driver::Ours(controller) => {
                    let update = controller
                        .feedback(&report, epoch + Duration::from_micros(at.unsigned_abs()));
                    update.probe.map(|probe| Armed {
                        id: probe.cluster.0,
                        rate: probe.rate.get(),
                        left: cluster_datagrams(probe.rate, DATAGRAM),
                        probe: PacedPacketInfo::new(
                            i32::try_from(probe.cluster.0).unwrap(),
                            i64::try_from(cluster_datagrams(probe.rate, DATAGRAM)).unwrap(),
                            0,
                        ),
                    })
                }
                Driver::Oracle(controller, target) => {
                    let feedbacks = covered
                        .iter()
                        .filter_map(|ext| {
                            let sent = *oracle_sent.get(ext)?;
                            Some(PacketResult {
                                sent_packet: sent,
                                receive_time: receiver
                                    .arrivals
                                    .get(ext)
                                    .filter(|arrival| **arrival <= at - DELAY)
                                    .map_or_else(Timestamp::plus_infinity, |arrival| {
                                        Timestamp::from_micros(*arrival)
                                    }),
                                ..Default::default()
                            })
                        })
                        .collect();
                    let update =
                        controller.on_transport_packets_feedback(TransportPacketsFeedback {
                            feedback_time: Timestamp::from_micros(at),
                            packet_feedbacks: feedbacks,
                            ..Default::default()
                        });
                    if let Some(rate) = update.target_rate {
                        *target = u32::try_from(rate.target_rate.bps()).unwrap_or(u32::MAX);
                    }
                    arm_oracle_probe(&update)
                }
            });
        }

        if now >= next_tick {
            next_tick += 250_000;
            if let Driver::Ours(controller) = &mut driver {
                let update = controller.tick(epoch + Duration::from_micros(now.unsigned_abs()));
                armed = armed.or_else(|| {
                    update.probe.map(|probe| Armed {
                        id: probe.cluster.0,
                        rate: probe.rate.get(),
                        left: cluster_datagrams(probe.rate, DATAGRAM),
                        probe: PacedPacketInfo::new(
                            i32::try_from(probe.cluster.0).unwrap(),
                            i64::try_from(cluster_datagrams(probe.rate, DATAGRAM)).unwrap(),
                            0,
                        ),
                    })
                });
            }
        }

        if now >= next_process
            && let Driver::Oracle(controller, target) = &mut driver
        {
            next_process += 25_000;
            let update = controller.on_process_interval(ProcessInterval {
                at_time: Timestamp::from_micros(now),
                pacer_queue: None,
            });
            if let Some(rate) = update.target_rate {
                *target = u32::try_from(rate.target_rate.bps()).unwrap_or(u32::MAX);
            }
            armed = armed.or_else(|| arm_oracle_probe(&update));
        } else if now >= next_process {
            next_process += 25_000;
        }

        if now % 100_000 < STEP {
            samples.push((now, driver.target()));
        }
    }

    Trace {
        samples,
        delivered,
        dropped,
    }
}

/// Turns the reference's probe request into a cluster this simulation can
/// release.
fn arm_oracle_probe(update: &goog_cc::transport::NetworkControlUpdate) -> Option<Armed> {
    let config = update.probe_cluster_configs.first()?;
    let rate = u32::try_from(config.target_data_rate.bps()).ok()?;
    let rate = NonZeroU32::new(rate)?;
    let datagrams = cluster_datagrams(rate, DATAGRAM);
    Some(Armed {
        id: u32::try_from(config.id).unwrap_or(0),
        rate: rate.get(),
        left: datagrams,
        probe: PacedPacketInfo::new(
            config.id,
            i64::from(config.target_probe_count).max(1),
            (config.target_data_rate * config.target_duration)
                .bytes()
                .max(1),
        ),
    })
}

/// Ten megabits, cut to two at ten seconds, restored at twenty.
const CLIFF: [(i64, u64); 3] = [
    (0, 10_000_000),
    (10_000_000, 2_000_000),
    (20_000_000, 10_000_000),
];

#[test]
fn a_session_climbs_to_what_the_link_carries() {
    let trace = simulate(&[(0, 10_000_000)], 8, Driver::ours(Instant::now()));
    let settled = trace.at(6_000_000);
    assert!(
        (6_000_000..=11_000_000).contains(&settled),
        "a ten megabit link settled at {settled}"
    );
    // The whole justification for building a probe sender: without probing,
    // 1.08 per second takes ln(5)/ln(1.08) = 20.9 seconds to climb from two
    // megabits to ten.
    let climbed = trace
        .first(0, |rate| rate > 6_000_000)
        .expect("the link must be found");
    assert!(
        climbed < 3_000_000,
        "probing must find a ten megabit link in under three seconds, and took {}ms",
        climbed / 1000
    );
    assert!(
        trace.dropped * 20 < trace.delivered,
        "{} of {} datagrams were dropped by the queue",
        trace.dropped,
        trace.delivered
    );
}

#[test]
fn a_link_that_collapses_is_detected_and_a_link_that_recovers_is_used() {
    let trace = simulate(&CLIFF, 30, Driver::ours(Instant::now()));

    let fell = trace
        .first(10_000_000, |rate| rate < 3_000_000)
        .expect("an eighty percent cut must be noticed");
    assert!(
        fell - 10_000_000 < 2_000_000,
        "the cut took {}ms to notice",
        (fell - 10_000_000) / 1000
    );

    let recovered = trace
        .first(20_000_000, |rate| rate > 6_000_000)
        .expect("a link that recovered must be used");
    assert!(
        recovered - 20_000_000 < 8_000_000,
        "recovery took {}ms, against the 20.9 seconds multiplicative increase \
         alone would take",
        (recovered - 20_000_000) / 1000
    );
}

#[test]
fn the_target_never_runs_away_from_the_link() {
    let trace = simulate(&CLIFF, 30, Driver::ours(Instant::now()));
    // Between two and nine seconds after the cut, nothing should still be
    // asking for the old capacity.
    for at in (12_000_000..20_000_000).step_by(500_000) {
        let rate = trace.at(at);
        assert!(
            rate < 4_000_000,
            "at {}s the target was {rate} on a two megabit link",
            at / 1_000_000
        );
        assert!(rate >= MIN_TARGET, "the target fell below the floor");
    }
}

#[test]
fn ours_and_the_reference_agree_about_the_link() {
    let ours = simulate(&CLIFF, 30, Driver::ours(Instant::now()));
    let reference = simulate(&CLIFF, 30, Driver::oracle());

    // Both must find the ten megabit link, and both must give it up when it
    // collapses. Agreement is asserted about the link rather than about a
    // number, because the two differ deliberately in one place: the
    // acknowledged rate here is a sliding window and libwebrtc's is a Bayesian
    // filter.
    for (name, trace) in [("ours", &ours), ("the reference", &reference)] {
        let found = trace.at(8_000_000);
        assert!(
            found > 4_000_000,
            "{name} settled at {found} on a ten megabit link"
        );
        let cut = trace.at(18_000_000);
        assert!(
            cut < 4_000_000,
            "{name} was still asking for {cut} on a two megabit link"
        );
    }

    // And within a factor of three at every sample away from the transitions,
    // which is where a difference would mean one of them is reading the path
    // wrongly rather than reacting at a different speed.
    for at in (3_000_000..19_500_000).step_by(500_000) {
        if (9_500_000..13_000_000).contains(&at) {
            continue;
        }
        let (mine, theirs) = (f64::from(ours.at(at)), f64::from(reference.at(at)));
        let ratio = (mine / theirs).max(theirs / mine);
        assert!(
            ratio < 3.0,
            "at {}s ours said {mine} and the reference said {theirs}",
            at / 1_000_000
        );
    }
}

#[test]
fn recovery_on_a_busy_sender_is_where_ours_leaves_the_reference() {
    // The one deliberate divergence, and the reason for it. GoogCC probes in
    // steady state only while the sender is application-limited, and
    // `NetworkControllerInterface` exposes no way to tell it that it is: the
    // signal comes from libwebrtc's own pacer, which is not on the other side
    // of this interface. So a sender producing exactly its target never probes,
    // and a link that recovers under it is never found again.
    //
    // This controller drops that gate. Probing at twice the rate for 15 ms
    // every 5 seconds costs 0.6% of the link whatever the link is, and a user
    // who is doing something is precisely the case where recovery matters.
    let ours = simulate(&CLIFF, 30, Driver::ours(Instant::now()));
    let reference = simulate(&CLIFF, 30, Driver::oracle());

    let back = ours.at(29_000_000);
    assert!(
        back > 6_000_000,
        "ours must take the recovered link back, and sat at {back}"
    );
    assert!(
        back >= reference.at(29_000_000),
        "ours must recover at least as fast as the reference"
    );
}

/// Drives one usable report at `recv_us` covering `count` datagrams released
/// from sequence `base`, so a controller can be exercised without the full link.
fn feed(controller: &mut Controller, epoch: Instant, base: u16, count: u16, sent_us: i64, recv_us: i64) {
    for offset in 0..count {
        controller.released(&Release {
            seq: Seq(base.wrapping_add(offset)),
            at: epoch + Duration::from_micros((sent_us + i64::from(offset) * 120).unsigned_abs()),
            wire: u16::try_from(DATAGRAM).unwrap(),
            cluster: None,
        });
    }
    let report = Feedback {
        base: Seq(base),
        reference: u32::try_from(recv_us / 250).unwrap_or(u32::MAX),
        packets: (0..count)
            .map(|index| PacketArrival::Received(if index == 0 { 0 } else { 4 }))
            .collect(),
        lost: 0,
        received: u64::from(base) + u64::from(count),
    };
    controller.feedback(&report, epoch + Duration::from_micros(recv_us.unsigned_abs()));
}

#[test]
fn feedback_going_silent_decays_the_target_to_the_floor() {
    // The session climbs into a ten megabit link, then the return path dies at
    // eight seconds and never speaks again.
    let trace = run(
        &[(0, 10_000_000)],
        22,
        Driver::ours(Instant::now()),
        Some((8_000_000, i64::MAX)),
    );

    // One second into the silence, before the threshold, the link is still held.
    let held = trace.at(9_000_000);
    assert!(
        held > 6_000_000,
        "the estimate must not decay before the threshold, and was {held}"
    );

    // And it reaches the floor within a bounded time once the threshold passes.
    let floored = trace
        .first(8_000_000, |rate| rate <= MIN_TARGET + 50_000)
        .expect("a dead return path must decay the target to the floor");
    assert!(
        floored - 8_000_000 < 16_000_000,
        "the decay took {}ms after the path died",
        (floored - 8_000_000) / 1000
    );
}

#[test]
fn a_return_path_that_recovers_takes_the_link_back() {
    // Dead from eight to sixteen seconds, then feedback resumes on the same
    // ten megabit link.
    let trace = run(
        &[(0, 10_000_000)],
        30,
        Driver::ours(Instant::now()),
        Some((8_000_000, 16_000_000)),
    );

    let low = trace.at(15_500_000);
    assert!(
        low < 4_000_000,
        "the silence must have pulled the target down, and it was {low}"
    );

    let back = trace
        .first(16_000_000, |rate| rate > 6_000_000)
        .expect("a link whose feedback resumed must be taken back");
    assert!(
        back - 16_000_000 < 10_000_000,
        "recovery took {}ms after feedback resumed",
        (back - 16_000_000) / 1000
    );
}

#[test]
fn a_tick_with_feedback_flowing_never_decays_the_estimate() {
    // The regression guard: while reports arrive, a tick is exactly the no-op it
    // was before the silence backstop existed.
    let epoch = Instant::now();
    let mut controller = Controller::new(NonZeroU32::new(CEILING).unwrap(), epoch);
    let mut next_tick = 250_000i64;
    let mut base = 0u16;
    let mut now = 0i64;
    while now < 8_000_000 {
        now += 50_000;
        feed(&mut controller, epoch, base, 12, now, now);
        base = base.wrapping_add(12);
        while now >= next_tick {
            let before = controller.estimate();
            controller.tick(epoch + Duration::from_micros(next_tick.unsigned_abs()));
            next_tick += 250_000;
            assert_eq!(
                before,
                controller.estimate(),
                "a tick moved the estimate while reports were flowing"
            );
        }
    }
}

#[test]
fn a_stall_drops_the_estimate_to_the_floor_in_one_call() {
    let epoch = Instant::now();
    let mut controller = Controller::new(NonZeroU32::new(CEILING).unwrap(), epoch);
    feed(&mut controller, epoch, 0, 12, 0, 100_000);
    assert!(
        controller.estimate().get() > MIN_TARGET,
        "the session must open above the floor"
    );

    controller.stalled();
    assert_eq!(
        controller.estimate().get(),
        MIN_TARGET,
        "a stall must collapse the estimate to the floor at once"
    );
}
