//! Serving one client.
//!
//! Capture, encode, transport and injection each know their own job. This is
//! where they meet, and where the policy that decides what the encoder is asked
//! for actually runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tachyx_media::{Bitrate, Capture, EncodeConfig, EncodeSize, Encoder, Idle, Rate};
use tachyx_proto::control::{
    Aspect, ClientMessage, Codec, CodedSize, HEARTBEAT_INTERVAL, Heartbeat, HostMessage,
    Size as WireSize, StreamConfig,
};
use tachyx_proto::media::Timestamp;
use tachyx_proto::packet::AccessUnit as WireUnit;
use tachyx_transport::endpoint::HostSession;

use crate::config::Config;
use crate::idr::{Budget, Reason, Verdict};
use crate::ladder::{Ladder, Settings, Size as RungSize};

/// How often the rate controller looks at what the link is carrying.
const RATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How often the send gate is re-examined while nothing is being encoded.
///
/// Short enough that a gate which shut on a burst reopens within a frame or two
/// of the queue draining.
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
    /// Input could not be injected.
    #[error(transparent)]
    Input(#[from] tachyx_input::Error),
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
    mut session: HostSession,
    injector: &mut tachyx_input::Injector,
) -> Result<(), SessionError> {
    // Held for the whole session. A blanked screen stops producing frames, and
    // the one person who wants the machine awake cannot reach it.
    let _awake = crate::idle::Inhibitor::new()?;

    let Pipeline {
        capture,
        encoder,
        units,
        bitrate,
        framerate,
        capture_size,
    } = start(config, &session)?;
    let admission = encoder.admission();

    // The pacer releases datagrams at a rate derived from this. Without it the
    // transport falls back to the ceiling the client announced, which is an
    // over-estimate and paces looser than the session actually needs.
    session.video.set_target_bitrate(
        std::num::NonZeroU32::new(bitrate.kbps() * 1000).ok_or(SessionError::Unsendable)?,
    );

    session
        .to_client
        .send(&HostMessage::Stream(stream_config(&encoder, bitrate)?))
        .await?;

    run(
        config,
        session,
        injector,
        capture,
        encoder,
        units,
        admission,
        bitrate,
        framerate,
        capture_size,
    )
    .await
}

/// What bringing the pipeline up produces.
struct Pipeline {
    capture: Capture,
    encoder: Arc<Encoder>,
    units: tachyx_media::AccessUnits,
    bitrate: Bitrate,
    framerate: tachyx_media::Framerate,
    capture_size: tachyx_media::Size,
}

/// Starts capture and encode for what this client says it can decode.
fn start(config: &Config, session: &HostSession) -> Result<Pipeline, SessionError> {
    let (capture, frames) = Capture::start(config.output.as_deref().unwrap_or_default())?;
    let format = capture.format().clone();
    let capture_size = format.size;

    let wanted = session.hello.capabilities.viewport.height.get();
    let wanted = u32::from(wanted)
        .min(u32::from(
            session.hello.capabilities.max_decode.height.get(),
        ))
        .min(capture_size.height());
    let size = EncodeSize::fit(capture_size, halving_for(capture_size.height(), wanted))?;

    // The encoder divides its bit budget by the rate declared here and ignores
    // when frames actually arrive, so it has to be a rate frames really arrive
    // at. That is the output's refresh, lowered by whatever the client asked for
    // and whatever the configuration allows: sending a phone 75 frames a second
    // when it asked for 60 spends a fifth of the link on frames it will not show.
    let asked = u32::from(session.hello.capabilities.requested_fps.get());
    let allowed = config.max_framerate.unwrap_or(u32::MAX);
    let framerate = tachyx_media::Framerate::from_millihertz(
        format
            .refresh
            .millihertz()
            .min(asked * 1000)
            .min(allowed * 1000),
    )
    .unwrap_or(format.refresh);
    let bitrate = Bitrate::from_kbps(
        config
            .max_bitrate
            .min(session.hello.capabilities.bitrate_ceiling.get())
            / 1000,
    )
    .ok_or(tachyx_media::BadEncodeSize::Odd)?;

    let (encoder, units) = Encoder::start(
        frames,
        EncodeConfig {
            size,
            rate: Rate { bitrate, framerate },
        },
    )?;
    Ok(Pipeline {
        capture,
        encoder: Arc::new(encoder),
        units,
        bitrate,
        framerate,
        capture_size,
    })
}

/// Runs the session until the client goes away.
#[expect(
    clippy::too_many_arguments,
    reason = "the pipeline's parts, threaded through one call rather than bundled into a struct that exists only to be unpacked"
)]
async fn run(
    _config: &Config,
    mut session: HostSession,
    injector: &mut tachyx_input::Injector,
    capture: Capture,
    encoder: Arc<Encoder>,
    units: tachyx_media::AccessUnits,
    admission: Arc<std::sync::atomic::AtomicBool>,
    bitrate: Bitrate,
    framerate: tachyx_media::Framerate,
    capture_size: tachyx_media::Size,
) -> Result<(), SessionError> {
    let mut ladder = Ladder::new(rung(capture_size), fps(framerate), bitrate.kbps() * 1000);
    let mut budget = Budget::new(bitrate.kbps() * 1000);
    // The picture the session opens on. Spent here so that a later request
    // cannot be refused on the grounds that this one was free: the budget is
    // shared precisely so every cause is counted.
    budget.request(Reason::Startup, Instant::now());
    let started = Instant::now();
    let sent_frames = Arc::new(AtomicU64::new(0));

    let pump = {
        let sent_frames = Arc::clone(&sent_frames);
        let admission = Arc::clone(&admission);
        let mut video = session.video;
        tokio::task::spawn_blocking(move || {
            loop {
                match units.recv_timeout(GATE_INTERVAL) {
                    Ok(unit) => {
                        let slices = unit.slices();
                        let Ok(wire) = WireUnit::new(unit.frame, unit.captured, unit.kind, &slices)
                        else {
                            continue;
                        };
                        // Only a lost connection ends the pump. A refusal is
                        // this frame's problem and the next one may well fit;
                        // treating it as fatal would end video for the life of
                        // the session over a transient.
                        if let Err(error) = video.send(&wire) {
                            tracing::warn!(%error, "video path ended");
                            break;
                        }
                        sent_frames.store(video.frames_sent(), Ordering::Relaxed);
                    }
                    Err(Idle::Waiting) => {}
                    Err(Idle::Ended) => break,
                }
                // Gate the NEXT frame before it is encoded. A frame never
                // submitted leaves the encoder's reference chain intact; a frame
                // encoded and then dropped breaks it, and with one reference
                // frame and a long key frame interval the picture stays wrong
                // for many seconds.
                //
                // Re-evaluated on the timeout as well as after a send, because a
                // shut gate stops the encoder, and a loop that only woke for a
                // frame would then never wake to open the gate again. That is a
                // deadlock, and it looks exactly like the picture freezing a few
                // seconds in.
                admission.store(
                    video.can_send_bytes(video.key_frame_estimate()),
                    Ordering::Relaxed,
                );
            }
            video
        })
    };

    let mut rate_tick = tokio::time::interval(RATE_INTERVAL);
    // The interval is the protocol's, not this daemon's: the client reads
    // liveness from a beat that did not arrive, so it has to expect the same
    // one the host keeps.
    let mut heartbeat_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    let connection = session.connection.clone();

    let outcome = loop {
        tokio::select! {
            message = session.from_client.recv::<ClientMessage>() => match message {
                Ok(ClientMessage::Input(batch)) => {
                    let events: Vec<_> =
                        batch.events.into_iter().map(|timed| timed.event).collect();
                    // A batch that cannot be injected is one batch lost. Ending
                    // the session over it would take the picture away as well,
                    // which is a far worse answer to a dropped click.
                    if let Err(error) = injector.apply(&events) {
                        tracing::warn!(%error, count = events.len(), "cannot inject input");
                    }
                }
                Ok(ClientMessage::KeyframeRequest { .. }) => {
                    if budget.request(Reason::ClientRequest, Instant::now()) == Verdict::Allow {
                        encoder.force_keyframe();
                    }
                }
                Ok(ClientMessage::ViewportChanged(changed)) => {
                    let wanted = u32::from(changed.viewport.height.get())
                        .min(u32::from(changed.max_decode.height.get()))
                        .min(capture_size.height());
                    if let Ok(size) =
                        EncodeSize::fit(capture_size, halving_for(capture_size.height(), wanted))
                        && budget.request(Reason::Resolution, Instant::now()) == Verdict::Allow
                    {
                        encoder.set_resolution(size)?;
                        let config = stream_config(&encoder, bitrate)?;
                        session.to_client.send(&HostMessage::Stream(config)).await?;
                    }
                }
                Err(_) => break Ok(()),
            },
            _ = rate_tick.tick() => {
                if let Some(target) = connection_target(&connection)
                    && let Some(Settings { size, bitrate: bits, .. }) =
                        ladder.observe(target, Instant::now())
                {
                    apply(&encoder, &mut budget, size, bits, &mut session.to_client).await?;
                }
            }
            _ = heartbeat_tick.tick() => {
                let stats = capture.stats();
                let beat = Heartbeat {
                    sent: Timestamp::from_session_micros(
                        u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    ),
                    last_frame: None,
                    frames_sent: sent_frames.load(Ordering::Relaxed),
                    frames_gated: stats.dropped,
                    pacing_rate: connection.congestion_state().metrics().pacing_rate,
                    pipeline_micros: 0,
                };
                if session.to_client.send(&HostMessage::Heartbeat(beat)).await.is_err() {
                    break Ok(());
                }
            }
        }
    };

    drop(encoder);
    drop(pump);
    capture.stop()?;
    outcome
}

/// The bandwidth the link will carry, in bits per second.
/// What the link will carry, if anything here can say.
///
/// Nothing here can, yet. The transport's own bandwidth estimate was measured
/// reporting zero on a healthy hundred-megabit path, for every sample of a
/// twenty-second run: its sampler discards any measurement taken while the
/// sender is limited by its own supply, and a sender producing sixty frames a
/// second is in that state whenever an acknowledgement arrives. Feeding that
/// zero to the ladder drove the encoder to its smallest picture on a link with
/// room to spare, which is worse than not adapting at all.
///
/// So this reports nothing until the receiver's own report exists, and the
/// ladder holds what the session was configured for. See `spec/congestion.md`.
const fn connection_target(_connection: &quinn::Connection) -> Option<u32> {
    None
}

/// Applies a rung, spending the key frame it costs from the shared budget.
async fn apply(
    encoder: &Encoder,
    budget: &mut Budget,
    size: RungSize,
    bits: u32,
    to_client: &mut tachyx_transport::control::ControlWriter,
) -> Result<(), SessionError> {
    let now = Instant::now();
    budget.retarget(bits);
    let Some(bitrate) = Bitrate::from_kbps(bits / 1000) else {
        return Ok(());
    };
    let encode = EncodeSize::new(u32::from(size.width), u32::from(size.height))?;
    let current = encoder.geometry().visible;
    if encode.visible() != current {
        if budget.request(Reason::Resolution, now) == Verdict::Refuse {
            return Ok(());
        }
        encoder.set_resolution(encode)?;
    } else if budget.request(Reason::Bitrate, now) == Verdict::Refuse {
        return Ok(());
    }
    encoder.set_rate(Rate {
        bitrate,
        framerate: encoder.rate().framerate,
    })?;
    to_client
        .send(&HostMessage::Stream(stream_config(encoder, bitrate)?))
        .await?;
    Ok(())
}

fn stream_config(encoder: &Encoder, bitrate: Bitrate) -> Result<StreamConfig, SessionError> {
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
        bitrate: std::num::NonZeroU32::new(bitrate.kbps() * 1000)
            .ok_or(SessionError::Unsendable)?,
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
    let mut height = capture_height;
    while height / 2 >= wanted.max(1) {
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
