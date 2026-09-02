//! Serving one client.
//!
//! Capture, encode, transport and injection each know their own job. This is
//! where they meet, and where the policy that decides what the encoder is asked
//! for actually runs.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tachyx_congestion::{Controller, MIN_TARGET, START_TARGET};
use tachyx_media::{Bitrate, Capture, EncodeConfig, EncodeSize, Encoder, Idle, Rate};
use tachyx_proto::control::{
    Aspect, ClientMessage, Codec, CodedSize, Feedback, Heartbeat, HostMessage, Size as WireSize,
    StreamConfig, HEARTBEAT_INTERVAL,
};
use tachyx_proto::media::Timestamp;
use tachyx_proto::packet::{AccessUnit as WireUnit, Parity};
use tachyx_transport::endpoint::HostSession;
use tachyx_transport::video::VideoControl;

use crate::config::Config;
use crate::cursor::{self, CursorFeed};
use crate::idr::{Budget, Reason, Verdict};
use crate::ladder::{Ladder, Produced, Settings, Size as RungSize};

/// How often the rate controller looks at what the link is carrying.
///
/// The estimate itself moves on every receiver report, twenty times a second.
/// This is the slower loop beside it: what the ladder reads, when a probe is
/// scheduled on a link nothing is reporting about, and the round trip the
/// additive increase is paced by.
const RATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The longest a picture may stay wrong before a key frame repairs it.
///
/// The encoder's own key frame interval counts FRAMES, which is the wrong unit
/// for this. Capture blocks until the screen changes, so a desktop nobody is
/// touching produces a couple of frames a second, and an interval of a hundred
/// and twenty frames is two seconds of a busy screen but a minute of a quiet
/// one. A frame lost during a quiet period would leave the picture frozen for
/// that minute, and the less that is happening the longer the freeze: the
/// opposite of what anyone would expect.
const KEY_FRAME_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the send gate is re-examined while nothing is being encoded.
///
/// Short enough that a gate which shut on a burst reopens within a frame or two
/// of the queue draining, and it is also the tick that finishes a probe cluster
/// with padding when no picture came along to be one.
const GATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Why a session ended.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Capture or encode failed.
    #[error(transparent)]
    Media(#[from] tachyx_media::Error),
    /// The encode size the client asked for is not one the encoder can code.
    #[error(transparent)]
    Size(#[from] tachyx_media::BadEncodeSize),
    /// The transport failed.
    #[error(transparent)]
    Transport(#[from] tachyx_transport::TransportError),
    /// The control stream failed.
    #[error(transparent)]
    Control(#[from] tachyx_transport::control::ControlError),
    /// The screen could not be kept awake.
    #[error(transparent)]
    Idle(#[from] crate::idle::Error),
    /// The encoder produced a picture the wire format will not carry.
    #[error("the encoder produced a picture that cannot be sent")]
    Unsendable,
}

/// Runs a session until the client goes away.
///
/// # Errors
/// If the pipeline cannot be started, or the connection fails for a reason
/// other than the client disconnecting.
pub async fn serve(
    config: &Config,
    session: HostSession,
    injection: &tachyx_input::Injection,
) -> Result<(), SessionError> {
    // Held for the whole session. A blanked screen stops producing frames, and
    // the one person who wants the machine awake cannot reach it.
    let _awake = crate::idle::Inhibitor::new()?;
    let pipeline = start(config, &session)?;
    run(session, injection, pipeline).await
}

/// What bringing the pipeline up produces.
struct Pipeline {
    capture: Capture,
    encoder: Arc<Encoder>,
    units: tachyx_media::AccessUnits,
    ladder: Ladder,
    /// The most this session may ever ask for, in bits a second.
    ceiling: NonZeroU32,
    capture_size: tachyx_media::Size,
    /// The output's own refresh, lowered by what the client asked for.
    refresh: u32,
}

/// Starts capture and encode for what this client says it can decode.
///
/// The ladder is built first and the encoder from what it chose, because the
/// opening point is a decision about the link rather than about the client:
/// a session that starts at the ceiling is the single largest cause of the
/// failure `spec/congestion.md` opens with.
fn start(config: &Config, session: &HostSession) -> Result<Pipeline, SessionError> {
    let (capture, frames) = Capture::start(config.output.as_deref().unwrap_or_default())?;
    let format = capture.format().clone();
    let capture_size = format.size;

    // The encoder divides its bit budget by the rate declared to it and ignores
    // when frames actually arrive, so the declared rate has to be one frames
    // really arrive at. That is the output's refresh, lowered by whatever the
    // client asked for and whatever the configuration allows: sending a phone
    // 75 frames a second when it asked for 60 spends a fifth of the link on
    // frames it will not show.
    let asked = u32::from(session.hello.capabilities.requested_fps.get());
    let allowed = config.max_framerate.unwrap_or(u32::MAX);
    let refresh = format
        .refresh
        .millihertz()
        .min(asked.saturating_mul(1000))
        .min(allowed.saturating_mul(1000))
        .max(1);

    let ceiling = NonZeroU32::new(
        config
            .max_bitrate
            .min(session.hello.capabilities.bitrate_ceiling.get())
            .max(MIN_TARGET),
    )
    .ok_or(SessionError::Unsendable)?;

    let ladder = Ladder::new(
        top_rung(
            capture_size,
            viewport(session.hello.capabilities, capture_size),
        )?,
        refresh,
        START_TARGET.min(ceiling.get()),
        ceiling.get(),
    );
    let opened = ladder.settings();
    let (encoder, units) = Encoder::start(frames, encode_config(capture_size, opened)?)?;
    Ok(Pipeline {
        capture,
        encoder: Arc::new(encoder),
        units,
        ladder,
        ceiling,
        capture_size,
        refresh,
    })
}

/// Everything that decides what the encoder is asked for, and how fast the
/// transport lets it out.
///
/// It is one object because every one of these moves the others: a receiver
/// report moves the estimate, the estimate moves the pacer, the token bucket
/// and the parity level, and what the encoder then produces moves the ladder,
/// which spends the same key frame budget a client's own request spends.
struct Live {
    encoder: Arc<Encoder>,
    controller: Controller,
    ladder: Ladder,
    budget: Budget,
    parity: Parity,
    control: VideoControl,
    /// The bits a second the encoder's own rate limiter is allowed to produce.
    spending: Arc<AtomicU32>,
    /// The transport's admission gate, which the watchdog reopens.
    admission: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    padded: Arc<AtomicU64>,
    watchdog: Watchdog,
    last_key: Instant,
    started: Instant,
    ceiling: NonZeroU32,
    refresh: u32,
    capture_size: tachyx_media::Size,
}

impl Live {
    /// Takes one receiver report, which is the only measurement of the path
    /// this host can have.
    fn feedback(&mut self, report: &Feedback, now: Instant) {
        self.collect();
        let update = self.controller.feedback(report, now);
        // Adaptive parity, which is where this beats the prior art: Sunshine
        // and Moonlight carry a flat fifth for ever, and this follows what the
        // link is really losing.
        self.parity
            .observe(update.losses.lost, update.losses.received);
        self.control.set_parity(self.parity.level());
        self.spend(update);
    }

    /// Looks at the link without a report to read, and moves the ladder.
    ///
    /// A still desktop produces no frames, so the client has nothing to report
    /// the arrival of and the controller would otherwise never hear that the
    /// link recovered. This is what schedules the padding probe that breaks
    /// that silence.
    async fn tick(
        &mut self,
        connection: &quinn::Connection,
        to_client: &mut tachyx_transport::control::ControlWriter,
        now: Instant,
    ) -> Result<(), SessionError> {
        self.collect();
        // What paces the additive increase. The transport measures it and the
        // controller has no other way to know it.
        self.controller.set_rtt(connection.rtt());
        let update = self.controller.tick(now);
        self.spend(update);

        let produced = produced(&self.encoder);
        let Some(want) = self.ladder.observe(update.video.get(), produced, now) else {
            return Ok(());
        };
        self.apply(want, to_client, now).await
    }

    /// Hands the controller everything the send path has released.
    fn collect(&mut self) {
        for release in self.control.released() {
            self.controller.released(&release);
        }
    }

    /// Tells the pacer and the encoder's rate limiter what the link will carry,
    /// and releases whatever cluster the controller asked for.
    ///
    /// Both the pacer and the bucket, because they bound different things and
    /// neither bounds the other. The pacer decides how fast bytes leave; the
    /// bucket decides how many the encoder is allowed to make. Nothing written
    /// into the encoder itself bounds either.
    fn spend(&mut self, update: tachyx_congestion::Update) {
        self.set_target(update.video);
        if let Some(probe) = update.probe {
            self.control.probe(probe);
        }
    }

    /// Tells the pacer and the encoder's rate limiter what the link will carry.
    fn set_target(&self, target: NonZeroU32) {
        self.control.set_target_bitrate(target);
        self.spending.store(target.get(), Ordering::Relaxed);
    }

    /// Applies a rung, spending the key frame it costs from the shared budget.
    ///
    /// The size and the rate go in together deliberately: measured on a running
    /// pipeline, a resolution change and a bitrate change applied in the same
    /// tick cost **one** key frame between them, not two, so the whole operating
    /// point moves in a single step and the client sees one [`StreamConfig`].
    async fn apply(
        &mut self,
        want: Settings,
        to_client: &mut tachyx_transport::control::ControlWriter,
        now: Instant,
    ) -> Result<(), SessionError> {
        let held = self.ladder.applied();
        let reason = if want.size == held.size {
            Reason::Bitrate
        } else {
            Reason::Resolution
        };
        if self.spend_key_frame(reason, now) == Verdict::Refuse {
            return Ok(());
        }
        if want.size != held.size {
            self.encoder.set_resolution(EncodeSize::new(
                u32::from(want.size.width),
                u32::from(want.size.height),
            )?)?;
        }
        self.encoder.set_rate(rate_of(want)?)?;
        self.budget.retarget(want.bitrate);
        self.ladder.acknowledge(want, now);
        self.last_key = now;
        to_client
            .send(&HostMessage::Stream(stream_config(
                &self.encoder,
                want.bitrate,
            )?))
            .await?;
        Ok(())
    }

    /// Asks the budget for a key frame, telling it first what one now costs.
    ///
    /// The budget opens on a 1080p estimate of 130 kB, and its refusal
    /// interval is that size over a fixed share of the target bitrate. Left
    /// unmeasured, a 720p session at the 2 Mbit/s opening target refuses every
    /// repair for 3.5 seconds, which was measured as 413 of 848 frames thrown
    /// away undecodable: each break in the reference chain froze the picture
    /// for the whole refusal interval, at twenty frames a second.
    fn spend_key_frame(&mut self, reason: Reason, now: Instant) -> Verdict {
        answer(
            &mut self.budget,
            self.encoder.stats().key_frame_bytes,
            reason,
            now,
        )
    }

    /// The client lost a frame and cannot continue from what it holds.
    fn keyframe_request(&mut self, now: Instant) {
        if self.spend_key_frame(Reason::ClientRequest, now) == Verdict::Allow {
            self.encoder.force_keyframe();
            self.last_key = now;
        }
    }

    /// The client can show something different now.
    ///
    /// The rungs are what the client can show, so a different client needs a
    /// different ladder. Its opening point is what the link is known to carry,
    /// which by now is a measurement rather than a guess.
    fn viewport_changed(&mut self, capabilities: tachyx_proto::control::Capabilities) {
        let wanted = viewport(capabilities, self.capture_size);
        if let Ok(top) = top_rung(self.capture_size, wanted) {
            self.ladder = Ladder::new(
                top,
                self.refresh,
                self.controller.video_target().get(),
                self.ceiling.get(),
            );
        }
    }

    /// Asks for a key frame when the last one is old enough that a loss nobody
    /// reported would still be showing.
    ///
    /// On a clock rather than a frame count. The encoder applies the request to
    /// the next picture it encodes, so a screen producing nothing pays nothing:
    /// the cost lands on the first frame after the quiet, which is exactly the
    /// frame a client needs to be able to decode.
    fn repair_if_stale(&mut self, now: Instant) {
        if now.duration_since(self.last_key) >= KEY_FRAME_MAX_AGE
            && self.spend_key_frame(Reason::Periodic, now) == Verdict::Allow
        {
            self.encoder.force_keyframe();
            self.last_key = now;
        }
    }

    /// Honours whatever the budget refused earlier and can now afford.
    ///
    /// A refusal that nothing acts on later is a change that never happens. The
    /// client repeats its own request, because it holds no record of having
    /// been refused; the host's own causes have nobody to repeat them.
    fn honour_owed(&mut self, now: Instant) {
        let Some(owed) = self.budget.owed() else {
            return;
        };
        match owed {
            Reason::ClientRequest | Reason::Periodic | Reason::Startup => {
                if self.spend_key_frame(owed, now) == Verdict::Allow {
                    self.encoder.force_keyframe();
                    self.last_key = now;
                }
            }
            // The ladder still wants the change and offers it again on its own
            // tick, where it is applied as well as paid for. A key frame forced
            // here would spend the allowance on nothing.
            Reason::Bitrate | Reason::Resolution => {}
        }
    }

    /// Notices a connection that is up and carrying nothing.
    fn watch(&mut self, now: Instant) {
        let stats = self.encoder.stats();
        if !self
            .watchdog
            .stalled(self.sent.load(Ordering::Relaxed), &stats, now)
        {
            return;
        }
        tracing::warn!(
            sent = self.sent.load(Ordering::Relaxed),
            encoded = stats.encoded,
            refused = stats.refused,
            gated = stats.gated,
            "the video path is carrying nothing while the connection is up"
        );
        // The path is confirmed dead: pictures were offered and none reached the
        // wire. Collapse the operating point to the floor before the recovery
        // key frame, so the key frame is small enough to fit a congestion window
        // no acknowledgement has grown, and so the host stops flooding a link
        // whose return path cannot get a word in until it does.
        self.controller.stalled();
        self.set_target(self.controller.video_target());
        // Not through the budget. It rations key frames against what the link
        // can carry, and this fires only when nothing is reaching the link at
        // all, so there is nothing to ration against and the client is holding a
        // picture it cannot decode from. The gate is reopened for one frame;
        // the send path checks the queue again for itself, so opening it cannot
        // reach the transport's eviction path.
        self.admission.store(true, Ordering::Relaxed);
        self.encoder.force_keyframe();
        self.last_key = now;
    }

    /// What the host has been doing, for a client that cannot tell a still
    /// screen from a host that has gone away.
    fn beat(&self, capture: &Capture, connection: &quinn::Connection) -> Heartbeat {
        let stats = self.encoder.stats();
        Heartbeat {
            sent: Timestamp::from_session_micros(
                u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
            ),
            last_frame: None,
            frames_sent: self.sent.load(Ordering::Relaxed),
            frames_gated: capture.stats().dropped + stats.gated,
            frames_refused: stats.refused,
            pacing_rate: connection.congestion_state().metrics().pacing_rate,
            estimate: Some(self.controller.estimate().get()),
            padding_sent: self.padded.load(Ordering::Relaxed),
            pipeline_micros: 0,
        }
    }
}

/// Runs the session until the client goes away.
async fn run(
    session: HostSession,
    injection: &tachyx_input::Injection,
    pipeline: Pipeline,
) -> Result<(), SessionError> {
    let Pipeline {
        capture,
        encoder,
        units,
        ladder,
        ceiling,
        capture_size,
        refresh,
    } = pipeline;
    let HostSession {
        connection,
        video,
        control,
        mut to_client,
        mut from_client,
        ..
    } = session;

    let started = Instant::now();
    let sent = Arc::new(AtomicU64::new(0));
    let padded = Arc::new(AtomicU64::new(0));
    let admission = encoder.admission();
    let pump = release(units, video, &admission, &sent, &padded);

    let mut live = Live {
        spending: encoder.budget(),
        controller: Controller::new(ceiling, started),
        budget: Budget::new(ladder.applied().bitrate),
        parity: Parity::default(),
        watchdog: Watchdog::new(started),
        last_key: started,
        encoder,
        ladder,
        control,
        admission,
        sent,
        padded,
        started,
        ceiling,
        refresh,
        capture_size,
    };
    // The picture the session opens on. Spent here so that a later request
    // cannot be refused on the grounds that this one was free: the budget is
    // shared precisely so every cause is counted.
    live.budget.request(Reason::Startup, started);
    let opening = live.controller.tick(started);
    live.spend(opening);
    to_client
        .send(&HostMessage::Stream(stream_config(
            &live.encoder,
            live.ladder.applied().bitrate,
        )?))
        .await?;

    let mut rate_tick = ticker(RATE_INTERVAL);
    // The interval is the protocol's, not this daemon's: the client reads
    // liveness from a beat that did not arrive, so it has to expect the same
    // one the host keeps.
    let mut heartbeat_tick = ticker(HEARTBEAT_INTERVAL);
    let mut cursor = CursorFeed::new(capture_size);
    let mut cursor_tick = ticker(cursor::INTERVAL);
    let mut key_frame_tick = ticker(KEY_FRAME_MAX_AGE / 4);

    let outcome = loop {
        tokio::select! {
            message = from_client.recv::<ClientMessage>() => match message {
                Ok(ClientMessage::Input(batch)) => {
                    // Handed over rather than applied here. Injection flushes a
                    // Wayland connection and sometimes waits on a full round
                    // trip to the compositor, and this loop also carries the
                    // video feedback, the cursor and the heartbeats.
                    injection.send(batch.events.into_iter().map(|timed| timed.event).collect());
                }
                Ok(ClientMessage::Feedback(report)) => live.feedback(&report, Instant::now()),
                Ok(ClientMessage::KeyframeRequest { .. }) => {
                    live.keyframe_request(Instant::now());
                }
                Ok(ClientMessage::ViewportChanged(changed)) => live.viewport_changed(changed),
                Err(_) => break Ok(()),
            },
            _ = rate_tick.tick() => {
                live.tick(&connection, &mut to_client, Instant::now()).await?;
            }
            _ = key_frame_tick.tick() => live.repair_if_stale(Instant::now()),
            _ = cursor_tick.tick() => {
                // The frame carries no pointer: capture is asked not to paint
                // one, so that the client can draw it where its own finger says
                // it is rather than a round trip behind.
                if let Some(feed) = cursor.as_mut()
                    && let Some(update) = feed.poll(&capture.cursor())
                    && to_client.send(&HostMessage::Cursor(update)).await.is_err()
                {
                    break Ok(());
                }
            }
            _ = heartbeat_tick.tick() => {
                live.watch(Instant::now());
                let beat = live.beat(&capture, &connection);
                if to_client.send(&HostMessage::Heartbeat(beat)).await.is_err() {
                    break Ok(());
                }
            }
        }
        // Read after the wait, not before it. A clock sampled ahead of the
        // select is as old as whatever woke the loop, which on a quiet session
        // is a whole heartbeat interval, and the budget then prices every
        // deferred key frame against a moment that has already passed.
        live.honour_owed(Instant::now());
    };

    drop(live);
    drop(pump);
    capture.stop()?;
    outcome
}

/// A periodic tick that does not try to catch up.
///
/// Every tick in this loop carries a control message, and tokio's default is to
/// fire once for each tick a stall went past. That answers a stall with a burst
/// of cursor updates and heartbeats queued in front of the video the stall
/// already delayed. Each of these reports the state at the moment it runs, so a
/// tick that was missed has nothing left to say and the phase is what matters.
fn ticker(period: std::time::Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker
}

/// Puts encoded pictures on the wire, on a thread of its own.
///
/// It has to be its own thread: `send` blocks until the pacer has released the
/// last datagram of the picture, which for a key frame is tens of milliseconds.
/// Nothing that produces frames is held while it waits, because capture writes
/// into a slot that displaces rather than queues.
fn release(
    units: tachyx_media::AccessUnits,
    mut video: tachyx_transport::video::VideoSender,
    admission: &Arc<std::sync::atomic::AtomicBool>,
    sent: &Arc<AtomicU64>,
    padded: &Arc<AtomicU64>,
) -> tokio::task::JoinHandle<tachyx_transport::video::VideoSender> {
    let admission = Arc::clone(admission);
    let sent = Arc::clone(sent);
    let padded = Arc::clone(padded);
    tokio::task::spawn_blocking(move || {
        loop {
            match units.recv_timeout(GATE_INTERVAL) {
                Ok(unit) => {
                    let slices = unit.slices();
                    let Ok(wire) = WireUnit::new(unit.frame, unit.captured, unit.kind, &slices)
                    else {
                        continue;
                    };
                    // Only a lost connection ends this. A refusal is this
                    // frame's problem and the next one may well fit; treating it
                    // as fatal would end video for the life of the session over
                    // a transient.
                    if let Err(error) = video.send(&wire) {
                        tracing::warn!(%error, "video path ended");
                        break;
                    }
                }
                // No picture came along to be the probe, which on a desktop
                // nobody is touching is the ordinary case. Padding is what times
                // the path when the picture is supplying nothing.
                Err(Idle::Waiting) => {
                    if let Err(error) = video.pad() {
                        tracing::warn!(%error, "video path ended");
                        break;
                    }
                }
                Err(Idle::Ended) => break,
            }
            // Gate the NEXT frame before it is encoded. A frame never submitted
            // leaves the encoder's reference chain intact; a frame encoded and
            // then dropped breaks it, and with one reference frame and a long
            // key frame interval the picture stays wrong for many seconds.
            //
            // Re-evaluated on the timeout as well as after a frame, because a
            // shut gate stops the encoder, and a loop that only woke for a frame
            // would then never wake to open the gate again. That is a deadlock,
            // and it looks exactly like the picture freezing a few seconds in.
            admission.store(
                video.can_send_bytes(video.delta_estimate()),
                Ordering::Relaxed,
            );
            sent.store(video.frames_sent(), Ordering::Relaxed);
            padded.store(video.padding_sent(), Ordering::Relaxed);
        }
        video
    })
}

/// The verdict for one cause of a key frame, priced at what the last key frame
/// really cost.
///
/// The measurement goes in on the same call that spends, rather than on a tick
/// of its own, so the budget can never refuse on a price the encoder has
/// already disproved. `measured` is zero until a key frame has been encoded,
/// and the budget's opening estimate stands until then.
fn answer(budget: &mut Budget, measured: u64, reason: Reason, now: Instant) -> Verdict {
    if let Some(bytes) = u32::try_from(measured).ok().filter(|bytes| *bytes > 0) {
        budget.observe(bytes);
    }
    budget.request(reason, now)
}

/// What the encoder has done, as the ladder reads it.
fn produced(encoder: &Encoder) -> Produced {
    let stats = encoder.stats();
    Produced {
        // Every capture frame the rate limiter was asked about. Frames dropped
        // to honour the declared frame rate are not among them: that is the
        // ladder's own decision being carried out, not a shortage.
        offered: stats.encoded + stats.refused + stats.gated,
        refused: stats.refused,
        bits: stats.bytes * 8,
    }
}

/// Notices a connection that is up and carrying nothing.
///
/// A still desktop produces no frames at all, so silence on the video path is
/// not a fault by itself. What is a fault is the encoder being offered pictures
/// and none of them reaching the wire: whatever shut the path, the client is
/// holding a picture it can no longer decode from.
#[derive(Debug)]
struct Watchdog {
    checked: Instant,
    sent: u64,
    offered: u64,
}

/// How long the path may carry nothing while the pipeline is producing.
const STALL: std::time::Duration = std::time::Duration::from_secs(1);

impl Watchdog {
    const fn new(now: Instant) -> Self {
        Self {
            checked: now,
            sent: 0,
            offered: 0,
        }
    }

    fn stalled(&mut self, sent: u64, stats: &tachyx_media::EncodeStats, now: Instant) -> bool {
        if now.duration_since(self.checked) < STALL {
            return false;
        }
        let offered = stats.encoded + stats.refused + stats.gated;
        let stalled = sent == self.sent && offered > self.offered;
        self.checked = now;
        self.sent = sent;
        self.offered = offered;
        stalled
    }
}

/// The rungs this client can show, as an encode size.
fn top_rung(
    capture: tachyx_media::Size,
    wanted: u32,
) -> Result<RungSize, tachyx_media::BadEncodeSize> {
    let fitted = EncodeSize::fit(capture, halving_for(capture.height(), wanted))?;
    Ok(rung(fitted.visible()))
}

/// The tallest picture this client can put to use.
fn viewport(capabilities: tachyx_proto::control::Capabilities, capture: tachyx_media::Size) -> u32 {
    // The height the picture will actually occupy, not the height of the client's
    // window. A wide desktop shown on a phone is letterboxed: the width runs out
    // first, so the drawn height is the client's width scaled by the capture's
    // own shape, and the bars above and below it are not picture.
    //
    // Taking the window height instead is how a 3440x1440 desktop was sent whole
    // to a phone that draws it 979 pixels tall. Those pixels are not free: they
    // are four times the encoding, four times the wire, and the loss that follows
    // from asking a link for four times what it carries breaks the encoder's
    // reference chain, which is what turns a busy link into a frozen picture.
    let box_width = u32::from(capabilities.viewport.width.get());
    let box_height = u32::from(capabilities.viewport.height.get());
    let drawn =
        u64::from(box_width) * u64::from(capture.height()) / u64::from(capture.width().max(1));
    u32::try_from(drawn)
        .unwrap_or(box_height)
        .min(box_height)
        .min(u32::from(capabilities.max_decode.height.get()))
        .min(capture.height())
}

/// The encoder configuration for one rung of the ladder.
fn encode_config(
    capture: tachyx_media::Size,
    settings: Settings,
) -> Result<EncodeConfig, SessionError> {
    Ok(EncodeConfig {
        size: EncodeSize::fit(capture, u32::from(settings.size.height))?,
        rate: rate_of(settings)?,
    })
}

/// The bitrate and declared frame rate a rung asks for.
fn rate_of(settings: Settings) -> Result<Rate, SessionError> {
    Ok(Rate {
        bitrate: Bitrate::from_kbps(settings.bitrate / 1000).ok_or(SessionError::Unsendable)?,
        framerate: tachyx_media::Framerate::from_millihertz(settings.millihertz)
            .ok_or(SessionError::Unsendable)?,
    })
}

fn stream_config(encoder: &Encoder, bitrate: u32) -> Result<StreamConfig, SessionError> {
    let geometry = encoder.geometry();
    let rate = encoder.rate();
    Ok(StreamConfig {
        codec: Codec::H264,
        coded: CodedSize::new(
            narrow(geometry.coded.width()),
            narrow(geometry.coded.height()),
        )
        .ok_or(SessionError::Unsendable)?,
        visible: WireSize::new(
            narrow(geometry.visible.width()),
            narrow(geometry.visible.height()),
        )
        .ok_or(SessionError::Unsendable)?,
        display_aspect: Aspect::new(
            narrow(geometry.display_aspect.0),
            narrow(geometry.display_aspect.1),
        )
        .ok_or(SessionError::Unsendable)?,
        framerate: std::num::NonZeroU16::new(fps(rate.framerate))
            .ok_or(SessionError::Unsendable)?,
        bitrate: NonZeroU32::new(bitrate).ok_or(SessionError::Unsendable)?,
    })
}

fn rung(size: tachyx_media::Size) -> RungSize {
    RungSize {
        width: u16::try_from(size.width()).unwrap_or(u16::MAX),
        height: u16::try_from(size.height()).unwrap_or(u16::MAX),
    }
}

/// The height of the largest halving of the capture that a viewport this tall
/// can show.
///
/// Scaling by a fraction that is not a power of two puts the scaler on an
/// expensive filter path: three quarters was measured costing more than not
/// scaling at all, and more than twice what a halving costs. Choosing the
/// nearest arbitrary fit is how a phone ended up being served the slowest size
/// of the three.
fn halving_for(capture_height: u32, wanted: u32) -> u32 {
    let wanted = wanted.max(1);
    let mut height = capture_height;
    // Nearest, not the smallest that still covers the viewport. Covering it
    // sounds right and is not: a 1800-tall desktop on a 1080-tall phone has no
    // halving above 1080, so "cover it" sends the full picture, 2.8 times the
    // pixels the screen can show. Those pixels are not free. They are what
    // makes the transport refuse frames, and a frame refused after it has been
    // encoded breaks the encoder's reference chain, so everything after it is
    // undecodable until the next key frame.
    //
    // A halving below the viewport is upscaled by the client and, on a screen
    // held at arm's length, invisibly so. A halving above it costs four times
    // the pixels for detail the panel cannot resolve.
    while height / 2 >= wanted {
        height /= 2;
    }
    // Between two halvings, take whichever is the smaller stretch. The break
    // even point is the geometric mean, so halve when the viewport sits below
    // `height / sqrt(2)`, written here without a square root.
    if 2 * u64::from(wanted) * u64::from(wanted) < u64::from(height) * u64::from(height) {
        height /= 2;
    }
    height
}

fn fps(rate: tachyx_media::Framerate) -> u16 {
    u16::try_from(rate.millihertz() / 1000).unwrap_or(u16::MAX)
}

/// Narrows a dimension to what the wire carries.
///
/// Saturating rather than wrapping: a picture wider than sixty-five thousand
/// pixels is not something this protocol carries, and reporting the ceiling is
/// a refusal the caller can act on rather than a small number that looks valid.
fn narrow(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{answer, halving_for, Budget, Reason, Verdict};

    /// The session measured on the phone: 1720x720 at the 2 Mbit/s opening
    /// target, a break in the reference chain, and a client repeating its
    /// request every 250 ms. Priced at the budget's 1080p opening estimate the
    /// refusal lasts 3.5 seconds, and the client showed 413 of 848 frames
    /// thrown away undecodable, ten frames of freeze per break. Priced at what
    /// the 720p key frame really cost, the repair is answered on the next
    /// request.
    #[test]
    fn a_repair_at_a_narrow_target_is_priced_at_the_measured_key_frame() {
        let start = Instant::now();
        let mut budget = Budget::new(2_000_000);
        budget.request(Reason::Startup, start);

        // The opening key frame the startup request bought: 26 kB measured at
        // 1720x720 and 2 Mbit/s, an eighth of the 130 kB opening estimate.
        let measured = 26_000;

        // The chain breaks one second in, and the client asks four times a
        // second until the picture is repaired.
        let broken = start + Duration::from_secs(1);
        let answered = (0..8u32)
            .map(|ask| broken + Duration::from_millis(250) * ask)
            .find(|now| {
                answer(&mut budget, measured, Reason::ClientRequest, *now) == Verdict::Allow
            })
            .map(|now| now.duration_since(broken));
        assert!(
            answered.is_some_and(|waited| waited <= Duration::from_secs(1)),
            "a repair the link can afford within a second was refused: {answered:?}"
        );
    }

    #[test]
    fn a_viewport_no_halving_covers_is_not_answered_with_the_whole_picture() {
        // The case that sent a phone 2.8 times the pixels it can show: an
        // 1800-tall capture has no halving between 900 and 1800, and answering
        // "cover the viewport" means answering 1800.
        assert_eq!(halving_for(1800, 1080), 900);
    }

    #[test]
    fn a_viewport_close_to_a_halving_gets_that_halving() {
        assert_eq!(halving_for(1440, 720), 720);
        assert_eq!(halving_for(1800, 900), 900);
        assert_eq!(halving_for(2160, 1080), 1080);
    }

    #[test]
    fn a_viewport_just_under_a_halving_keeps_it_rather_than_dropping_a_whole_step() {
        // 1080 of 1440 is a stretch of 1.33; halving to 720 would be 1.5.
        assert_eq!(halving_for(1440, 1080), 1440);
    }

    #[test]
    fn the_answer_is_always_a_halving_of_the_capture() {
        for wanted in (100..2000).step_by(37) {
            let height = halving_for(1800, wanted);
            assert!(
                [1800, 900, 450, 225, 112, 56, 28, 14, 7, 3, 1].contains(&height),
                "{wanted} gave {height}"
            );
        }
    }

    #[test]
    fn a_viewport_larger_than_the_capture_gets_the_capture() {
        assert_eq!(halving_for(1800, 1800), 1800);
        assert_eq!(halving_for(1800, 4000), 1800);
    }
}
