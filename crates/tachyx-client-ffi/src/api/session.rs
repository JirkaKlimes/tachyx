//! Opening a session, feeding it what the user does, and reading what it saw.

use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use flutter_rust_bridge::frb;
use tachyx_client::AudioEvent;
use tachyx_crypto::{DeviceId, RelayAddress};
use tachyx_driver::{AudioSink, FrameSink, Link};
use tachyx_proto::control::{AudioMode, AudioStatus, Size};
pub use tachyx_proto::input::InputEvent;
use tachyx_proto::media::Timestamp;

use crate::api::audio::AudioStats;
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
#[derive(Debug, Clone)]
pub struct Encoding {
    /// The codec the host chose, in the name the viewer knows it by.
    pub codec: String,
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
    /// What the session is doing about sound, and what the play path counted.
    pub audio: AudioStats,
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
    /// The first thing the play path complained about, if it has.
    ///
    /// A machine with no sound server says so here. The session goes on: a
    /// remote desktop with no sound is a working remote desktop, and the user is
    /// told rather than left wondering.
    pub audio_fault: Option<String>,
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

/// The speakers the driver pushes sound into.
///
/// The player is built on the first packet rather than at connect, for the same
/// reason [`Present`] builds its pipeline there: opening a sound device takes as
/// long as the machine's sound server takes to answer, and a session should be
/// running before that is discovered. Nothing is lost by the wait either, since
/// there is nothing to play until there is a packet.
#[derive(Debug)]
struct Listen {
    player: OnceLock<tachyx_audio::AudioPlayer>,
    /// Where the host's reported volume is read from. Weak, because the link
    /// owns this sink and an owning handle back would be a cycle neither end
    /// escaped.
    link: Weak<Link>,
    /// Set once the device has refused, so a machine with no sound server is
    /// asked once rather than a hundred times a second.
    refused: OnceLock<String>,
    /// The gain last handed to the player, as the bits of an `f32`. It starts
    /// where the player starts, so a host reporting full volume changes nothing.
    gain: AtomicU32,
}

impl AudioSink for Listen {
    fn push(&self, event: AudioEvent) {
        let Some(player) = self.player() else {
            return;
        };
        self.follow_the_host(player);
        player.push(event);
    }
}

impl Listen {
    /// The player, built on the first thing there is to play.
    fn player(&self) -> Option<&tachyx_audio::AudioPlayer> {
        if let Some(player) = self.player.get() {
            return Some(player);
        }
        if self.refused.get().is_some() {
            return None;
        }
        match tachyx_audio::AudioPlayer::new() {
            Ok(player) => {
                let _ = self.player.set(player);
                self.player.get()
            }
            Err(error) => {
                let _ = self.refused.set(error.to_string());
                None
            }
        }
    }

    /// Plays at whatever volume the host last reported.
    ///
    /// The host's sink volume is the session's only volume: the capture tap is
    /// ahead of it, so the stream carries full scale sound and this is where the
    /// setting is applied. Read on every packet and handed on only when it has
    /// changed, because the player ramps to a new gain and a gain set again is
    /// a ramp to where it already is.
    fn follow_the_host(&self, player: &tachyx_audio::AudioPlayer) {
        let Some(gain) = self
            .link
            .upgrade()
            .and_then(|link| link.audio_status())
            .map(AudioStatus::gain)
        else {
            return;
        };
        if self.gain.swap(gain.to_bits(), Ordering::Relaxed) != gain.to_bits() {
            player.set_gain(gain);
        }
    }

    /// What the play path has counted, if there is one yet.
    fn stats(&self) -> tachyx_audio::Stats {
        self.player
            .get()
            .map(tachyx_audio::AudioPlayer::stats)
            .unwrap_or_default()
    }

    /// The first thing the play path complained about.
    fn failure(&self) -> Option<String> {
        self.player
            .get()
            .and_then(tachyx_audio::AudioPlayer::failure)
            .or_else(|| self.refused.get().cloned())
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
    /// Attached the moment the session opens. Sound needs nothing from the
    /// platform side to start, unlike a texture, so there is nothing for it to
    /// wait for.
    listen: Arc<Listen>,
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
        let playing = self.listen.stats();
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
            audio: AudioStats {
                host_has_audio: stats.host_audio,
                // Off until the host has said otherwise, which is what a client
                // that has heard nothing about sound is doing about it.
                mode: stats.audio.map(|sound| sound.mode).unwrap_or_default(),
                volume: stats.audio.map_or(0, |sound| sound.volume),
                muted: stats.audio.is_some_and(|sound| sound.muted),
                packets: stats.audio_packets,
                lost: stats.audio_lost,
                played: playing.played,
                concealed: playing.concealed,
                recovered: playing.recovered,
                dropped: playing.dropped,
                buffered: playing.buffered,
            },
            encoding: stats.stream.map(|stream| Encoding {
                codec: stream.codec.to_string(),
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
            audio_fault: self.listen.failure(),
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

    /// Asks the host to start streaming its sound, stop, or change what it does
    /// with its own output meanwhile.
    ///
    /// Nothing happens on a host whose hello said it has no audio, which
    /// [`AudioStats::host_has_audio`] reports and the application reads before
    /// it offers the control. What the host is really doing comes back in
    /// [`AudioStats::mode`], because a host may refuse.
    ///
    /// Synchronous: it is a toolbar tap, and the answer arrives on the next
    /// statistics poll rather than out of this call.
    #[frb(sync)]
    pub fn set_audio_mode(&self, mode: AudioMode) {
        self.link.set_audio_mode(mode);
    }

    /// Sets the **host's** sink volume, in permille of its own hundred percent.
    ///
    /// There is one volume in the session and it is the host's. The capture tap
    /// sits ahead of it, so the stream carries the same samples whatever this is
    /// set to and the client applies the reported value as playback gain. One
    /// knob, and it works the same in every mode.
    ///
    /// A value above a thousand is bounded on the way out. What reads it cubes
    /// it, so four times the maximum would be a sixty-fourfold gain in
    /// somebody's ears.
    #[frb(sync)]
    pub fn set_host_volume(&self, permille: u16) {
        self.link.set_host_volume(permille);
    }

    /// Mutes or unmutes the **host's** own sink.
    ///
    /// The host's silence, not the client's: the tap is ahead of the mute, so a
    /// muted host still streams everything it is playing. A client that read
    /// [`AudioStats::muted`] as its own silence would hear nothing in the one
    /// mode chosen to make it hear everything.
    #[frb(sync)]
    pub fn set_host_muted(&self, muted: bool) {
        self.link.set_host_muted(muted);
    }

    /// Tells the host the client can take something different now, after a
    /// rotation, a resize, or a change of resolution mode.
    ///
    /// `bitrate_ceiling` caps what the stream may cost, in bits per second.
    /// `None` keeps the client's default. A client shrunk into a
    /// picture-in-picture window passes a low one, because a thumbnail is not
    /// worth a full-rate stream.
    ///
    /// # Errors
    /// If the viewport has no area or the display never refreshes.
    #[frb(sync)]
    pub fn viewport_changed(
        &self,
        viewport_width: u16,
        viewport_height: u16,
        refresh_hz: u16,
        bitrate_ceiling: Option<u32>,
    ) -> anyhow::Result<()> {
        self.link.viewport_changed(asked_for(
            viewport_width,
            viewport_height,
            refresh_hz,
            bitrate_ceiling,
        )?);
        Ok(())
    }

    fn present(&self) -> std::sync::MutexGuard<'_, Option<Arc<Present>>> {
        self.present.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Opens a session with the host at `address`, for a video surface of this size
/// in physical pixels on a display that refreshes this often.
///
/// `device` is the host's identifier in base32, as [`known_hosts`] reports it.
/// Not an address: a host is reached by trying the addresses it gave at pairing
/// and then by asking a relay, and either way the identifier is the only half
/// that is checked. There is no trust on first use, so a host this client has
/// not paired with cannot be reached at all.
///
/// `state_directory` is this client's own directory. Its device key lives there,
/// and that key is what a host pairs with.
///
/// `relays` are the relays to ask when none of the host's own addresses answer,
/// each written `address:port#KEY`. The host's own relay is tried first
/// whatever this says.
///
/// `bitrate_ceiling` caps what the stream may cost, in bits per second. `None`
/// keeps the client's default.
///
/// # Errors
/// If `device` is not an identifier, this client has not paired with it, the
/// viewport has no area, a relay is not written the way a relay is written,
/// nothing answered, or the host refused. A refusal carries the host's own
/// words, so a version mismatch reads as one.
///
/// A host that has revoked this client refuses **after** the handshake looks
/// complete: TLS 1.3 sends the client's certificate in its own second flight,
/// so the dialling end learns it on the first read. That failure names
/// re-pairing rather than reading as a lost connection.
///
/// [`known_hosts`]: crate::api::pairing::known_hosts
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
pub fn connect(
    device: String,
    viewport_width: u16,
    viewport_height: u16,
    refresh_hz: u16,
    state_directory: String,
    relays: Vec<String>,
    bitrate_ceiling: Option<u32>,
) -> anyhow::Result<Session> {
    let capabilities = asked_for(viewport_width, viewport_height, refresh_hz, bitrate_ceiling)?;
    let asked = relay_list(&relays)?;
    let link = Arc::new(Link::connect(
        DeviceId::parse(&device)?,
        Path::new(&state_directory),
        &asked,
        capabilities,
    )?);
    // The speakers are attached here rather than on a call of their own. Sound
    // needs no handle from the platform side, so there is nothing for the
    // application to hand over and nothing for it to remember to do.
    let listen = Arc::new(Listen {
        player: OnceLock::new(),
        link: Arc::downgrade(&link),
        refused: OnceLock::new(),
        gain: AtomicU32::new(1.0f32.to_bits()),
    });
    link.listen_on(Arc::clone(&listen) as Arc<dyn AudioSink>);
    Ok(Session {
        link,
        present: Mutex::new(None),
        listen,
    })
}

/// Reads the relay list the settings screen keeps.
///
/// One that will not parse is an error naming it, rather than a relay quietly
/// dropped from the list: a user who mistyped one and is told nothing would
/// spend the fault on the host instead.
///
/// # Errors
/// If any entry is not written `address:port#KEY`.
pub(crate) fn relay_list(written: &[String]) -> anyhow::Result<Vec<RelayAddress>> {
    written
        .iter()
        .map(|relay| {
            relay.parse::<RelayAddress>().map_err(|_| {
                anyhow::anyhow!(
                    "{relay} is not a relay: one is written address:port#KEY, where KEY is \
                     the relay's identifier"
                )
            })
        })
        .collect()
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
    bitrate_ceiling: Option<u32>,
) -> anyhow::Result<tachyx_proto::control::Capabilities> {
    let viewport = Size::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("a {width}x{height} viewport has no picture in it"))?;
    let refresh = NonZeroU16::new(refresh_hz)
        .ok_or_else(|| anyhow::anyhow!("a display that never refreshes cannot show a stream"))?;
    let capabilities =
        tachyx_client::capabilities(viewport, refresh).decoding(tachyx_video::decoders()?);
    Ok(
        match bitrate_ceiling.and_then(|cap| NonZeroU32::new(cap.clamp(CAP_FLOOR, CAP_ROOF))) {
            Some(ceiling) => capabilities.ceiling(ceiling),
            None => capabilities,
        },
    )
}

/// The least a cap from the settings screen may ask for. Below this the ladder
/// bottoms out and the session is frozen stills; a user asking for less is
/// asking for a session that does not work.
const CAP_FLOOR: u32 = 500_000;

/// The most a cap may ask for, which is well past what any rung spends.
const CAP_ROOF: u32 = 100_000_000;
