//! Opening a session, feeding it what the user does, and reading what it saw.

use std::num::NonZeroU16;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use flutter_rust_bridge::frb;
use tachyx_driver::{FrameSink, Link};
use tachyx_proto::control::Size;
pub use tachyx_proto::input::InputEvent;
use tachyx_proto::media::Timestamp;

use crate::api::video::PresentStats;

/// Prepares the bridge, before Dart calls anything else.
///
/// Installs the panic hook that turns a Rust panic into a message in the
/// application's log rather than a silent abort on a background thread.
#[frb(init)]
pub fn init() {
    flutter_rust_bridge::setup_default_user_utils();
    init_logging();
}

/// Sends the client's own logs to the platform's log, once.
///
/// A phone has no stdout a developer can read, so without this the transport
/// and the reassembly say nothing on the device. The filter is generous on the
/// client's own crates and quiet elsewhere, and `try_init` means a second
/// bridge setup installs nothing rather than panicking.
fn init_logging() {
    #[cfg(target_os = "android")]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,tachyx_client=debug,tachyx_driver=debug,tachyx_transport=info",
            )
        });
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(paranoid_android::layer("tachyx"))
            .try_init();
    }
}

/// The protocol version this build speaks.
///
/// Shown on the connection screen, because a client and a host are updated
/// independently and a version mismatch is the first thing to check.
#[frb(sync)]
#[must_use]
pub fn protocol_version() -> u16 {
    tachyx_proto::VERSION
}

/// What the session is doing, as far as the client can tell.
///
/// Silence on the video path is not a fault. The host's capture blocks while the
/// screen is static, with idle gaps measured up to 1.53 seconds, so a still
/// screen says [`Liveness::Idle`] and only a host that has stopped answering
/// says [`Liveness::Lost`].
#[derive(Debug, Clone, Copy)]
pub enum Liveness {
    /// Pictures are arriving.
    Streaming,
    /// The host is answering and the screen is not changing.
    Idle,
    /// The host has said nothing for three heartbeat intervals.
    Lost,
}

impl From<tachyx_client::Liveness> for Liveness {
    fn from(liveness: tachyx_client::Liveness) -> Self {
        match liveness {
            tachyx_client::Liveness::Streaming => Self::Streaming,
            tachyx_client::Liveness::Idle => Self::Idle,
            tachyx_client::Liveness::Lost => Self::Lost,
        }
    }
}

/// What the client's own video path has seen.
///
/// Every field is something this client counted. Nothing here is a figure the
/// host reported about itself, and nothing is an estimate: a statistics overlay
/// built to measure latency is worth less than nothing if it shows a number that
/// was made up.
#[derive(Debug, Clone, Copy)]
pub struct VideoStats {
    /// Datagrams parsed.
    pub packets_received: u64,
    /// Datagrams the sequence number says never arrived.
    pub packets_lost: u64,
    /// Datagrams for a frame that had already been decided.
    pub packets_late: u64,
    /// Frames handed to the decoder.
    pub frames_delivered: u64,
    /// Frames abandoned with fragments missing.
    pub frames_dropped: u64,
    /// Frames the host numbered and of which nothing arrived at all.
    pub frames_missing: u64,
    /// Frames that arrived whole and were thrown away anyway, because the
    /// frames they refer back to did not.
    pub frames_undecodable: u64,
}

/// How the host said it is encoding.
///
/// The decoder needs none of this: the parameter sets travel in the bitstream
/// before every key frame, so a resolution change reconfigures it without a word
/// from here. What the application needs is the shape, which the bitstream does
/// not carry.
///
/// Not called `Stream`, because Dart already has one and a client that imported
/// both would have to say which it meant on every line.
#[derive(Debug, Clone, Copy)]
pub struct Encoding {
    /// The width the encoder codes.
    pub coded_width: u16,
    /// The height the encoder codes, which is the visible height rounded up to
    /// a multiple of sixteen.
    pub coded_height: u16,
    /// The visible width, which is what a pointer delta is measured in.
    pub visible_width: u16,
    /// The visible height.
    pub visible_height: u16,
    /// The width term of the shape to draw the picture at.
    pub aspect_width: u16,
    /// The height term of that shape.
    pub aspect_height: u16,
    /// The frame rate declared to the encoder.
    pub framerate: u16,
    /// The target bitrate, in bits per second.
    pub bitrate: u32,
}

/// What the host says about itself, which is the only second-hand thing here.
#[derive(Debug, Clone, Copy)]
pub struct HostReport {
    /// Frames the host put on the wire.
    pub frames_sent: u64,
    /// Frames the host declined to encode because its send buffer had no room
    /// for them whole.
    pub frames_gated: u64,
    /// The transport's own bandwidth estimate, in bits per second.
    pub pacing_rate: Option<u64>,
    /// Median capture to wire latency over the last interval, in microseconds.
    pub pipeline_us: u32,
}

/// Everything one session has to report.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Whether pictures are arriving, the screen is still, or the host is gone.
    pub liveness: Liveness,
    /// The transport's own round trip estimate, in microseconds.
    ///
    /// quinn measures it from its own acknowledgements, so it is the real path
    /// and costs no message of ours. It is a round trip and stays one: splitting
    /// it into two one-way times would need the two clocks related, which this
    /// milestone does not do.
    pub rtt_us: u32,
    /// Input batches put on the wire.
    pub batches: u64,
    /// What reassembly saw.
    pub video: VideoStats,
    /// How the host is encoding, once it has said.
    pub encoding: Option<Encoding>,
    /// What the host last said about itself.
    pub host: Option<HostReport>,
    /// What the decode and present path measured, once there is one.
    pub present: Option<PresentStats>,
    /// Why the session ended, if it has.
    pub ended: Option<String>,
    /// The first thing the decode path complained about, if it has.
    pub video_fault: Option<String>,
}

/// The decoder the driver pushes pictures into.
///
/// It records the first access unit the decoder would not take. A pipeline that
/// cannot negotiate reports itself through
/// [`tachyx_video::VideoTexture::failure`]; a decoder that refuses a frame for
/// any other reason would otherwise say nothing at all.
#[derive(Debug)]
struct Present {
    texture: tachyx_video::VideoTexture,
    /// Where the host's choice of codec is read from.
    ///
    /// Weak, because the link owns this sink: an owning handle back would be a
    /// cycle and neither end would ever be dropped.
    link: Weak<Link>,
    /// Set once the decoder has been told which codec to build for, whether or
    /// not it managed to.
    started: OnceLock<()>,
    refused: OnceLock<String>,
}

impl FrameSink for Present {
    fn push(&self, access_unit: Vec<u8>) {
        self.start_decoding();
        if let Err(refused) = self.texture.push(access_unit) {
            let _ = self.refused.set(refused.to_string());
        }
    }
}

impl Present {
    /// Builds the decode pipeline the first time the host's choice of codec is
    /// known.
    ///
    /// Not in [`Session::present_on`], because a texture is registered on
    /// Flutter's platform thread and may be registered before the host's first
    /// `StreamConfig` has arrived. Here it cannot be too early: the host sends
    /// that message on the control stream before it sends a picture, so by the
    /// time there is an access unit to push there is a codec to read. Anything
    /// pushed before the pipeline exists queues inside the texture.
    fn start_decoding(&self) {
        if self.started.get().is_some() {
            return;
        }
        let Some(codec) = self
            .link
            .upgrade()
            .and_then(|link| link.stats().stream)
            .map(|stream| stream.codec)
        else {
            return;
        };
        let _ = self.started.set(());
        if let Err(error) = self.texture.decode(codec) {
            let _ = self.refused.set(error.to_string());
        }
    }
}

/// Where to draw the host's pointer, in fractions of the video picture.
///
/// Capture leaves the cursor out of the picture on purpose, so this is what
/// puts it back, and it is drawn over the video rather than in it: the position
/// is the client's own, moved by the user's own touches the moment they happen
/// and corrected by what the host reports. Nothing here waits for a round trip.
///
/// Fractions rather than pixels because the host encodes at whatever rung of
/// the resolution ladder the link allows, and the picture is drawn into a box
/// of whatever size the window is. Both change under this without changing what
/// a fraction means.
#[derive(Debug, Clone)]
pub struct Cursor {
    /// The hotspot, from the left edge of the picture.
    pub x: f32,
    /// The hotspot, from the top edge.
    pub y: f32,
    /// Whether the host has confirmed this position.
    ///
    /// False on a compositor that offers no cursor session: the client is then
    /// steering a pointer nothing corrects, so it drifts, and a client that
    /// showed that silently would be lying about what it knows.
    pub confirmed: bool,
    /// The picture, once the host has sent one.
    ///
    /// `None` means draw a pointer of the client's own. Fetched through
    /// [`Session::cursor_image`] rather than carried here, because it changes
    /// thousands of times less often than the position and this is read once a
    /// frame.
    pub image: Option<CursorShape>,
}

/// The geometry of the pointer's picture, in fractions of the video picture.
#[derive(Debug, Clone, Copy)]
pub struct CursorShape {
    /// Which picture this is. It changes only when the picture does, so a
    /// drawing layer decodes each one once.
    ///
    /// It names a shape rather than counting changes: the pointer coming back
    /// to a shape it has held before comes back to the identifier it had.
    pub id: u32,
    /// The picture's width, as a fraction of the video's width.
    pub width: f32,
    /// Its height, as a fraction of the video's height.
    pub height: f32,
    /// How far left of the hotspot the picture starts.
    pub hotspot_x: f32,
    /// How far above the hotspot it starts.
    pub hotspot_y: f32,
}

/// The pointer's picture, ready for the platform to turn into an image.
#[derive(Debug, Clone)]
pub struct CursorImage {
    /// Which picture this is, matching [`CursorShape::id`].
    pub id: u32,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// `width * height` pixels, row by row, four bytes each: blue, green, red,
    /// then alpha, with the colour already multiplied by the alpha.
    pub pixels: Vec<u8>,
}

/// A live session with a host.
///
/// It holds [`tachyx_driver::Link`] and adds nothing but the translation: the
/// decisions live in `tachyx-client` and `tachyx-driver`, where a test reaches
/// them without a Dart runtime.
#[frb(opaque)]
#[derive(Debug)]
pub struct Session {
    /// Shared rather than owned outright, so that the sink can read the host's
    /// choice of codec back out of it. See [`Present::start_decoding`].
    link: Arc<Link>,
    present: Mutex<Option<Arc<Present>>>,
}

impl Session {
    /// Decodes into a Flutter texture and reports its identifier, for a
    /// `Texture` widget to draw.
    ///
    /// `engine_handle` comes from the `irondash_engine_context` Dart plugin.
    /// Synchronous so that it runs on the thread that called it, which is the
    /// only one that can reach the engine's texture registry.
    ///
    /// # Errors
    /// If GStreamer will not start, the engine refuses the texture, or this
    /// target presents some other way. What the codec the host chose costs is
    /// reported through [`SessionStats::video_fault`] instead, because the
    /// pipeline for it is built on the first picture rather than here.
    #[frb(sync)]
    pub fn present_on(&self, engine_handle: i64) -> anyhow::Result<i64> {
        let present = Arc::new(Present {
            texture: tachyx_video::VideoTexture::new(engine_handle)?,
            link: Arc::downgrade(&self.link),
            started: OnceLock::new(),
            refused: OnceLock::new(),
        });
        let id = present.texture.id();
        self.link
            .present_on(Arc::clone(&present) as Arc<dyn FrameSink>);
        *self.present() = Some(present);
        Ok(id)
    }

    /// What this session has done.
    ///
    /// Synchronous, because the overlay reads it several times a second and a
    /// round trip through the isolate to fetch a page of counters would cost
    /// more than the counters are worth.
    #[frb(sync)]
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        let stats = self.link.stats();
        let present = self.present().clone();
        SessionStats {
            liveness: stats.liveness.into(),
            rtt_us: u32::try_from(stats.rtt.as_micros()).unwrap_or(u32::MAX),
            batches: stats.batches,
            video: VideoStats {
                packets_received: stats.video.packets_received,
                packets_lost: stats.video.packets_lost,
                packets_late: stats.video.packets_late,
                frames_delivered: stats.video.frames_delivered,
                frames_dropped: stats.video.frames_dropped,
                frames_missing: stats.video.frames_missing,
                frames_undecodable: stats.video.frames_undecodable,
            },
            encoding: stats.stream.map(|stream| Encoding {
                coded_width: stream.coded.size().width.get(),
                coded_height: stream.coded.size().height.get(),
                visible_width: stream.visible.width.get(),
                visible_height: stream.visible.height.get(),
                aspect_width: stream.display_aspect.width.get(),
                aspect_height: stream.display_aspect.height.get(),
                framerate: stream.framerate.get(),
                bitrate: stream.bitrate.get(),
            }),
            host: stats.host.map(|beat| HostReport {
                frames_sent: beat.frames_sent,
                frames_gated: beat.frames_gated,
                pacing_rate: beat.pacing_rate,
                pipeline_us: beat.pipeline_micros,
            }),
            present: present
                .as_ref()
                .map(|present| present.texture.stats().into()),
            ended: stats.ended,
            video_fault: present.and_then(|present| {
                present
                    .texture
                    .failure()
                    .or_else(|| present.refused.get().cloned())
            }),
        }
    }

    /// Where to draw the host's pointer, or `None` when there is nothing to
    /// draw: before the host has said anything, or while the pointer is on
    /// another monitor.
    ///
    /// Synchronous, and read once a frame by the layer that draws it. A round
    /// trip through the isolate for a pointer position would be the one round
    /// trip this whole path exists to avoid.
    #[frb(sync)]
    #[must_use]
    pub fn cursor(&self) -> Option<Cursor> {
        self.link.cursor().map(|view| Cursor {
            x: view.at.x,
            y: view.at.y,
            confirmed: view.tracking == tachyx_client::Tracking::Reported,
            image: view.shape.map(|shape| CursorShape {
                id: shape.id.0,
                width: shape.width,
                height: shape.height,
                hotspot_x: shape.hotspot_x,
                hotspot_y: shape.hotspot_y,
            }),
        })
    }

    /// The rows of the pointer's current picture, if it is the one asked for.
    ///
    /// `id` is what [`CursorShape::id`] last reported. Passing it keeps the
    /// picture off this boundary except on the frame it changes, which is once
    /// every few seconds against sixty reads of the position a second.
    #[frb(sync)]
    #[must_use]
    pub fn cursor_image(&self, id: u32) -> Option<CursorImage> {
        let shape = self.link.cursor()?.shape?;
        if shape.id.0 != id {
            return None;
        }
        Some(CursorImage {
            id: shape.id.0,
            width: shape.image.width.get(),
            height: shape.image.height.get(),
            pixels: shape.image.pixels()?.to_vec(),
        })
    }

    /// Records what the user did.
    ///
    /// `at_micros` is when the client saw the event, taken from
    /// `PointerEvent.timeStamp` rather than from the moment this was called, so
    /// that batching does not hide inside the latency measurement.
    ///
    /// Synchronous, because a touch sample every eight milliseconds cannot
    /// afford a round trip through the isolate to be recorded.
    #[frb(sync)]
    pub fn input(&self, at_micros: u64, event: InputEvent) {
        self.link
            .input(Timestamp::from_session_micros(at_micros), event);
    }

    /// Tells the host the client can take something different now, after a
    /// rotation, a resize, or a change of resolution mode.
    ///
    /// # Errors
    /// If the viewport has no area or the display never refreshes.
    #[frb(sync)]
    pub fn viewport_changed(
        &self,
        viewport_width: u16,
        viewport_height: u16,
        refresh_hz: u16,
    ) -> anyhow::Result<()> {
        self.link
            .viewport_changed(asked_for(viewport_width, viewport_height, refresh_hz)?);
        Ok(())
    }

    fn present(&self) -> std::sync::MutexGuard<'_, Option<Arc<Present>>> {
        self.present.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Opens a session with the host at `address`, for a video surface of this size
/// in physical pixels on a display that refreshes this often.
///
/// `state_directory` is where the host's fingerprint is remembered, so that a
/// host presenting a different key on a later connection is refused rather than
/// silently accepted.
///
/// # Errors
/// If the address names nothing, the viewport has no area, the host presents a
/// key other than the one already pinned for it, or the host refuses the
/// session. A refusal carries the host's own words, so a version mismatch reads
/// as one.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
pub fn connect(
    address: String,
    viewport_width: u16,
    viewport_height: u16,
    refresh_hz: u16,
    state_directory: String,
) -> anyhow::Result<Session> {
    let capabilities = asked_for(viewport_width, viewport_height, refresh_hz)?;
    Ok(Session {
        link: Arc::new(Link::connect(
            &address,
            Path::new(&state_directory),
            capabilities,
        )?),
        present: Mutex::new(None),
    })
}

/// What the client can drive, once the numbers Dart measured are known to be
/// numbers.
///
/// The codec set is probed here rather than assumed anywhere below, because
/// this is the lowest layer that is allowed to touch the machine:
/// `tachyx-client` does no I/O at all and cannot ask.
fn asked_for(
    width: u16,
    height: u16,
    refresh_hz: u16,
) -> anyhow::Result<tachyx_proto::control::Capabilities> {
    let viewport = Size::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("a {width}x{height} viewport has no picture in it"))?;
    let refresh = NonZeroU16::new(refresh_hz)
        .ok_or_else(|| anyhow::anyhow!("a display that never refreshes cannot show a stream"))?;
    Ok(tachyx_client::capabilities(viewport, refresh).decoding(tachyx_video::decoders()?))
}
