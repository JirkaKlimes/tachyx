//! The `vah264enc` pipeline: `appsrc ! vapostproc ! capsfilter ! vah264enc ! appsink`.
//!
//! The source pushes the capture's own size and keeps that size for the life of
//! the pipeline, because a fixed-size capture cannot renegotiate. The resolution
//! ladder is a caps filter between the scaler and the encoder, which applies in
//! 0.03 ms and takes effect on the very next frame.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::DmaBufAllocator;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video as gst_video;
use tachyx_proto::media::{FrameId, FrameKind, Timestamp};

use crate::capture::{CaptureFormat, Frames};
use crate::slot::Slot;
use crate::{EncodeSize, Error, Rate, Size};

/// How long to wait for the encoder to answer for a frame that was pushed.
///
/// The chain is synchronous, so this is a bound on a stall rather than a normal
/// wait: an encoder that has stopped answering must be reported, not waited on.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);

/// Floor on how often a key frame may be forced.
///
/// The property's own default is no limit at all. A client that has lost
/// synchronisation cannot use anything before the next key frame, so the floor
/// only has to be short enough not to be felt, while still stopping a burst of
/// recovery requests from turning the whole stream into key frames.
const MIN_KEYFRAME_INTERVAL: Duration = Duration::from_millis(100);

/// The NAL unit type that carries padding and nothing else.
const NAL_FILLER: u8 = 12;

/// How long a still screen may bank credit for, as a fraction of a second.
///
/// The ceiling on the token bucket is the greater of this much of the target
/// and the key frame it has just measured, so a desktop that has been still for
/// a fifth of a second has banked enough to pay for a key frame outright. That
/// is exactly the case that matters: the user comes back, the screen changes, a
/// key frame is needed, and it must not be rationed at the moment the session
/// becomes interactive again.
const BUCKET_CEILING: Duration = Duration::from_millis(100);

/// How long a lost frame may leave the picture wrong.
///
/// The scheduled key frame is the only thing that ends the damage: this encoder
/// offers neither intra refresh nor long-term reference frames, so there is no
/// cheaper repair. A client that notices asks for one sooner; this bounds the
/// case where it does not notice, or where its request was refused.
const KEY_FRAME_INTERVAL_SECONDS: u32 = 2;

/// What to encode, and at what rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeConfig {
    /// Picture size to send. A lower rung costs the scaler work and saves the
    /// encoder more: measured on a 3440x1440 capture, the whole chain takes
    /// 7.56 ms at the capture's own size and 4.94 ms scaled to 2580x1080.
    pub size: EncodeSize,
    /// Bits per second and the frame rate they are divided by.
    pub rate: Rate,
}

/// The geometry a decoder needs, which the bitstream does not fully carry.
///
/// H.264 expresses a size the macroblock grid cannot hold by coding a larger
/// picture and attaching a crop, and the scaler's aspect correction never
/// reaches the sequence parameter set at all, so all three travel in the stream
/// configuration message instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGeometry {
    /// The size the bitstream codes.
    pub coded: Size,
    /// The size a decoder must show.
    pub visible: Size,
    /// Width to height of the visible picture, in lowest terms.
    pub display_aspect: (u32, u32),
}

/// One encoded picture, ready for the packetiser.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// Position in the encoded sequence, and the reassembly key on the wire.
    pub frame: FrameId,
    /// When the picture was captured.
    pub captured: Timestamp,
    /// Whether the picture stands alone.
    pub kind: FrameKind,
    /// The access unit in Annex B form, parameter sets included before every
    /// key frame, filler removed.
    pub bytes: Vec<u8>,
}

impl AccessUnit {
    /// The picture's independently decodable pieces, in decode order, ready to
    /// hand to [`tachyx_proto::packet::AccessUnit::new`] without copying.
    ///
    /// There is exactly one, and the array type says so rather than implying a
    /// choice that is not there. The encoder runs with `num-slices` at one
    /// because more slices measured slower, so an access unit is one piece and
    /// losing any datagram of it costs the whole picture. The protocol's slice
    /// field is carrying nothing while that holds.
    ///
    /// Bind the result before lending it, because the array is a temporary:
    /// `let slices = unit.slices();` then `AccessUnit::new(.., &slices)`.
    #[must_use]
    pub fn slices(&self) -> [&[u8]; 1] {
        [&self.bytes]
    }
}

/// The encoder's output.
#[derive(Debug)]
pub struct AccessUnits(Receiver<AccessUnit>);

/// Why waiting for an access unit produced none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// Nothing was encoded within the time allowed. The encoder is still there.
    ///
    /// This is the ordinary state of a still screen: capture blocks until
    /// something changes, so a desktop nobody is touching produces nothing for
    /// as long as a second and a half.
    Waiting,
    /// Encoding has ended and nothing further will arrive.
    Ended,
}

impl AccessUnits {
    /// Waits for the next access unit, or returns `None` once encoding has
    /// ended.
    #[must_use]
    pub fn recv(&self) -> Option<AccessUnit> {
        self.0.recv().ok()
    }

    /// Waits for the next access unit, giving up after `timeout`.
    ///
    /// A caller that also has periodic work must use this rather than [`recv`].
    /// A still screen produces no frames at all, so a loop that only wakes for a
    /// frame does not wake, and anything it was going to do on the way round
    /// never happens.
    ///
    /// [`recv`]: Self::recv
    ///
    /// # Errors
    /// [`Idle::Waiting`] when the time ran out, [`Idle::Ended`] when the encoder
    /// has stopped.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<AccessUnit, Idle> {
        self.0.recv_timeout(timeout).map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => Idle::Waiting,
            std::sync::mpsc::RecvTimeoutError::Disconnected => Idle::Ended,
        })
    }
}

/// Percentiles of a timing sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Percentiles {
    /// Median.
    pub p50: Duration,
    /// Ninetieth percentile.
    pub p90: Duration,
    /// Ninety-ninth percentile.
    pub p99: Duration,
    /// Worst sample held.
    pub max: Duration,
}

/// What the encoder has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncodeStats {
    /// Access units emitted.
    pub encoded: u64,
    /// Bytes emitted, after filler was removed.
    pub bytes: u64,
    /// Bytes of filler the encoder emitted and this crate discarded.
    pub filler_bytes: u64,
    /// Frames the token bucket refused, which were therefore never encoded.
    ///
    /// This is the rate limiter doing its job: the picture would have cost more
    /// than the link carries. Counted apart from [`EncodeStats::gated`] because
    /// the two name different faults, and the session that reported "179 gated"
    /// could not say which mechanism did it.
    pub refused: u64,
    /// Frames the transport's admission gate refused.
    ///
    /// A safety interlock against quinn's own eviction path, not a rate
    /// control. With the pacer keeping that queue one or two datagrams deep it
    /// should never fire, and a session in which it does is a session with
    /// something else wrong.
    pub gated: u64,
    /// Frames dropped to honour the declared frame rate.
    ///
    /// The encoder budgets its bits as the bitrate divided by the rate declared
    /// in the caps and ignores when frames actually arrive, so a declared rate
    /// the pump does not deliver at is a bitrate setting that means something
    /// else. This is the pump delivering at it.
    pub paced: u64,
    /// Time from handing a frame to the pipeline to holding its access unit.
    pub latency: Percentiles,
    /// Bytes of the most recent key frame, filler removed. Zero until one has
    /// been encoded.
    ///
    /// What the key frame budget in `tachyx-hostd` prices its refusals from. It
    /// opens on a 1080p estimate of 130 kB, and a 720p key frame at a narrow
    /// target is a fraction of that: left unmeasured, the budget refuses a
    /// client whose picture is broken for seconds longer than the link
    /// requires.
    pub key_frame_bytes: u64,
}

impl EncodeStats {
    /// Share of the encoder's output that was padding.
    #[must_use]
    pub fn filler_fraction(&self) -> f64 {
        let total = self.bytes + self.filler_bytes;
        if total == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a ratio does not need the last bit of a byte count"
        )]
        {
            self.filler_bytes as f64 / total as f64
        }
    }
}

/// A token bucket over the bytes the encoder actually produced.
///
/// This is the rate limiter, and the encoder's own `bitrate` property is not.
/// Measured on this hardware, `vah264enc` asked for 1 Mbit/s on content it
/// cannot compress produced 11.54, and asked for 20 Mbit/s on content it can
/// produced 0.21: the property spans two orders of magnitude of error in each
/// direction. **Nothing written into the encoder bounds what goes on the wire.
/// The only thing that bounds it is refusing to give the encoder pictures.**
///
/// It is charged the measured size rather than a modelled one, because the
/// model is wrong too. Measured, dropping capture frames ahead of the encoder:
/// halving the frame rate gives 62% of the bytes and quartering it gives 41%,
/// because the encoder lowers its quantiser when it finds itself under budget
/// and spends the slack on the frames that do arrive. For the controller that
/// is good news rather than bad, and it only means the bucket must be charged
/// what actually happened.
///
/// A frame refused here is refused **before** the encoder sees it, so the
/// reference chain is intact and the client is owed nothing. That is the whole
/// difference between this and a gate that drops encoded pictures.
#[derive(Debug)]
struct Bucket {
    balance: f64,
    refilled: Instant,
}

impl Bucket {
    const fn new(now: Instant) -> Self {
        Self {
            balance: 0.0,
            refilled: now,
        }
    }

    /// Whether the next capture frame may be given to the encoder.
    ///
    /// The balance may go negative by at most one frame, and the frames refused
    /// while it recovers cost nothing but themselves.
    fn admits(&mut self, target: u32, key_frame_bits: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        let target = f64::from(target);
        self.balance += target * elapsed;
        self.balance = self
            .balance
            .min((target * BUCKET_CEILING.as_secs_f64()).max(key_frame_bits));
        self.balance >= 0.0
    }

    /// Charges what the encoder really produced.
    fn charge(&mut self, bytes: usize) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame size is far below the exact integer range of a double"
        )]
        {
            self.balance -= (bytes as f64) * 8.0;
        }
    }
}

/// Delivers capture frames at the rate declared to the encoder, and no faster.
///
/// The encoder budgets bits per frame as the bitrate divided by the rate in the
/// caps and ignores presentation timestamps entirely, so caps declaring 60
/// while capture delivers 75 is a bitrate setting a quarter higher than the one
/// asked for. This is what makes the two agree.
///
/// It is a deadline rather than a count of every nth frame because capture
/// blocks while the screen is static: on an idle output frames arrive about
/// once a second, and dropping two of every three of those would make a screen
/// that has just started moving look broken.
#[derive(Debug, Default)]
struct Cadence(Option<Instant>);

impl Cadence {
    fn admits(&mut self, declared: crate::Framerate, now: Instant) -> bool {
        let interval =
            Duration::from_nanos(1_000_000_000_000 / u64::from(declared.millihertz().max(1)));
        if self
            .0
            .is_some_and(|last| now.saturating_duration_since(last) + SLACK < interval)
        {
            return false;
        }
        self.0 = Some(now);
        true
    }
}

/// How early a capture frame may arrive and still count for its slot.
///
/// Capture is refresh-locked and the deadline is a whole multiple of the
/// refresh interval, so without a little slack every frame arrives a few
/// microseconds early and the pump delivers at half the declared rate.
const SLACK: Duration = Duration::from_micros(500);

/// A running encode of one capture.
///
/// Dropping this stops the encode thread. The capture that feeds it is stopped
/// separately, and in either order.
#[derive(Debug)]
pub struct Encoder {
    control: Arc<Control>,
    counters: Arc<Counters>,
    admission: Arc<AtomicBool>,
    budget: Arc<AtomicU32>,
    source: Arc<Slot<crate::Frame>>,
    thread: Option<JoinHandle<Result<(), Error>>>,
}

impl Encoder {
    /// Builds the pipeline and starts a thread that drains `frames` into it.
    ///
    /// # Errors
    /// If GStreamer lacks an element, the pipeline will not start, or the
    /// capture's buffer layout cannot be described to it.
    pub fn start(frames: Frames, config: EncodeConfig) -> Result<(Self, AccessUnits), Error> {
        gst::init()?;
        let control = Arc::new(Control::build(frames.format(), config)?);
        let counters = Arc::new(Counters::default());
        let admission = Arc::new(AtomicBool::new(true));
        // Starts at what the session opened on, so an encoder nobody retargets
        // spends what it was configured for.
        let budget = Arc::new(AtomicU32::new(
            config.rate.bitrate.kbps().saturating_mul(1000),
        ));
        let (sender, receiver) = sync_channel(2);
        let source = frames.slot();

        let thread = {
            let control = Arc::clone(&control);
            let counters = Arc::clone(&counters);
            let admission = Arc::clone(&admission);
            let budget = Arc::clone(&budget);
            std::thread::Builder::new()
                .name("tachyx-encode".to_owned())
                .spawn(move || pump(&frames, &control, &counters, &admission, &budget, &sender))?
        };

        Ok((
            Self {
                control,
                counters,
                admission,
                budget,
                source,
                thread: Some(thread),
            },
            AccessUnits(receiver),
        ))
    }

    /// The bits a second the encoder is allowed to actually produce.
    ///
    /// This is the rate limiter. The `bitrate` property [`Encoder::set_rate`]
    /// writes is the operating point the encoder aims at, and it was measured
    /// missing by two orders of magnitude in each direction; a token bucket
    /// charged the measured size of every frame is what bounds the wire. See
    /// [`Bucket`].
    ///
    /// Shared rather than set, because the encode thread owns it for the life
    /// of the session and the controller that moves it does not.
    #[must_use]
    pub fn budget(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.budget)
    }

    /// The gate that decides whether the next frame is encoded at all.
    ///
    /// Clear it while the transport has no room for another frame. A frame
    /// refused here never reaches the encoder, so the reference chain is
    /// unbroken and every picture that does go out stays decodable. Encoding a
    /// frame and then dropping it on the wire would instead corrupt the picture
    /// until the next key frame, which with one reference frame and a key
    /// interval of 1024 is many seconds away.
    ///
    /// It starts set, so an encoder nobody gates encodes everything. Refusals
    /// are counted in [`EncodeStats::refused`], apart from the frames capture
    /// dropped at the slot.
    #[must_use]
    pub fn admission(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.admission)
    }

    /// The geometry the current settings produce.
    #[must_use]
    pub fn geometry(&self) -> StreamGeometry {
        let size = self.control.settings().size;
        StreamGeometry {
            coded: size.coded(),
            visible: size.visible(),
            display_aspect: size.display_aspect(),
        }
    }

    /// The rate the encoder is spending to.
    #[must_use]
    pub fn rate(&self) -> Rate {
        self.control.settings().rate
    }

    /// Sets the bitrate and the frame rate it is divided by.
    ///
    /// Both move together because the encoder spends `bitrate / framerate` on
    /// every picture and ignores presentation timestamps, so changing one alone
    /// scales the real rate by the ratio between them. This forces a key frame:
    /// the property setter marks the encoder for reconfiguration, and
    /// reconfiguration resets its state.
    ///
    /// # Errors
    /// If the source caps cannot be rebuilt.
    pub fn set_rate(&self, rate: Rate) -> Result<(), Error> {
        {
            let mut settings = self
                .control
                .settings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Writing the rate the element already has is not free: it has no
            // equality guard of its own, so the write raises its reconfigure
            // flag and the next picture is a key frame. Measured by writing
            // 20000 over 20000 and watching an IDR come out. A caller that
            // recomputes the same target every tick would otherwise spend the
            // whole session sending key frames.
            if settings.rate == rate {
                return Ok(());
            }
            settings.rate = rate;
        }
        self.control.apply_rate(rate)
    }

    /// Moves to another rung of the resolution ladder.
    ///
    /// Takes effect on the very next frame and drops nothing.
    ///
    /// # Errors
    /// If the filter caps cannot be built.
    pub fn set_resolution(&self, size: EncodeSize) -> Result<(), Error> {
        {
            let mut settings = self
                .control
                .settings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            settings.size = size;
        }
        self.control.apply_size(size);
        Ok(())
    }

    /// Asks for a key frame on the next picture, parameter sets included.
    pub fn force_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(pad) = self.control.encoder.static_pad("src") {
            pad.send_event(event);
        }
    }

    /// Counters since the pipeline started.
    #[must_use]
    pub fn stats(&self) -> EncodeStats {
        self.counters.snapshot()
    }

    /// Stops encoding and reports how the thread ended.
    ///
    /// # Errors
    /// Whatever ended the encode thread, unless it was this call.
    pub fn stop(mut self) -> Result<(), Error> {
        let thread = self.thread.take();
        self.source.close();
        match thread.map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(Error::Gst("the encode thread panicked".to_owned())),
            None => Ok(()),
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.source.close();
        if let Ok(Err(error)) = thread.join() {
            tracing::warn!(%error, "encoding ended in an error");
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    size: EncodeSize,
    rate: Rate,
}

/// The pipeline and everything needed to change its mind about it.
#[derive(Debug)]
struct Control {
    pipeline: gst::Pipeline,
    source: AppSrc,
    filter: gst::Element,
    encoder: gst::Element,
    sink: AppSink,
    settings: Mutex<Settings>,
    capture: Size,
    fourcc: u32,
    modifier: u64,
    length: usize,
    offsets: Vec<usize>,
    strides: Vec<i32>,
}

impl Control {
    fn build(format: &CaptureFormat, config: EncodeConfig) -> Result<Self, Error> {
        let source = AppSrc::builder()
            .format(gst::Format::Time)
            .is_live(true)
            .do_timestamp(false)
            .block(true)
            .build();
        source.set_max_buffers(1);
        let scaler = make("vapostproc")?;
        // The scaler drops frames to meet a deadline nobody set here, and
        // letterboxes rather than stretching, because the pixel aspect ratio it
        // would otherwise record never reaches the bitstream.
        scaler.set_property("qos", false);
        scaler.set_property("add-borders", true);
        let filter = make("capsfilter")?;
        let encoder = make("vah264enc")?;
        configure_encoder(&encoder);
        let sink = AppSink::builder()
            .sync(false)
            .max_buffers(1u32)
            .drop(false)
            .build();

        let pipeline = gst::Pipeline::new();
        let elements = [
            source.upcast_ref::<gst::Element>(),
            &scaler,
            &filter,
            &encoder,
            sink.upcast_ref::<gst::Element>(),
        ];
        pipeline.add_many(elements)?;
        gst::Element::link_many(elements)?;

        let control = Self {
            pipeline,
            source,
            filter,
            encoder,
            sink,
            settings: Mutex::new(Settings {
                size: config.size,
                rate: config.rate,
            }),
            capture: format.size,
            fourcc: format.fourcc,
            modifier: format.modifier,
            length: format.length,
            offsets: format
                .planes
                .iter()
                .map(|plane| plane.offset as usize)
                .collect(),
            strides: format
                .planes
                .iter()
                .map(|plane| i32::try_from(plane.stride).unwrap_or(i32::MAX))
                .collect(),
        };
        control.apply_size(config.size);
        control.apply_rate(config.rate)?;
        control.pipeline.set_state(gst::State::Playing)?;
        Ok(control)
    }

    fn settings(&self) -> Settings {
        *self.settings.lock().expect("settings mutex")
    }

    /// States the geometry the scaler must produce, and that it must stay on
    /// the GPU.
    ///
    /// The memory feature is load-bearing. Caps with no feature field mean
    /// system memory, and the scaler and the encoder both honour that: the
    /// negotiated caps become a bare `video/x-raw` and every picture makes a
    /// round trip through main memory. It costs nothing measurable on this
    /// integrated part, where the two memories are the same memory, but it
    /// would on anything with its own.
    ///
    /// Restating a geometry the scaler is not changing costs nothing either.
    /// Measured over three passes at 3440x1440, all at the refresh rate: 7.64,
    /// 7.62 and 7.57 ms with the geometry stated against 7.64, 7.64 and 7.64 ms
    /// with the filter left open. An earlier note in the findings claimed that
    /// restating it defeated a pass-through and doubled the cost. It does not.
    fn apply_size(&self, size: EncodeSize) {
        let caps = gst::Caps::builder("video/x-raw")
            .features(["memory:VAMemory"])
            .field("width", i32::try_from(size.visible().width()).unwrap_or(0))
            .field(
                "height",
                i32::try_from(size.visible().height()).unwrap_or(0),
            )
            .build();
        self.filter.set_property("caps", caps);
    }

    /// The frame rate reaches the encoder through the source caps because the
    /// scaler does not convert frame rate: a filter asking it for another rate
    /// fails to link.
    fn apply_rate(&self, rate: Rate) -> Result<(), Error> {
        self.source.set_caps(Some(&self.source_caps(rate)?));
        self.encoder.set_property("bitrate", rate.bitrate.kbps());
        // Half the bitrate is the floor. A smaller value is silently replaced
        // by twice the bitrate, which is also the default.
        self.encoder
            .set_property("cpb-size", rate.bitrate.kbps() / 2);
        Ok(())
    }

    fn source_caps(&self, rate: Rate) -> Result<gst::Caps, Error> {
        let format = gst_video::dma_drm_fourcc_to_format(self.fourcc)?;
        let info =
            gst_video::VideoInfo::builder(format, self.capture.width(), self.capture.height())
                .fps(rate.framerate.as_fraction())
                .build()?;
        Ok(gst_video::VideoInfoDmaDrm::from_video_info(&info, self.modifier)?.to_caps()?)
    }

    /// Wraps one shared buffer as a GStreamer buffer.
    ///
    /// Every plane of a buffer object shares one descriptor, so this is a
    /// single memory covering the object and a video meta saying where inside
    /// it each plane begins.
    fn wrap(
        &self,
        allocator: &DmaBufAllocator,
        fd: OwnedFd,
        captured: Timestamp,
    ) -> Result<gst::Buffer, Error> {
        let memory = tachyx_dmabuf::wrap_plane(allocator, fd, self.length)?;
        let mut buffer = gst::Buffer::new();
        let raw = buffer
            .get_mut()
            .expect("a buffer that was just made has one reference");
        raw.append_memory(memory);
        raw.set_pts(gst::ClockTime::from_useconds(
            captured.micros_since(Timestamp::from_session_micros(0)),
        ));
        gst_video::VideoMeta::add_full(
            raw,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::DmaDrm,
            self.capture.width(),
            self.capture.height(),
            &self.offsets,
            &self.strides,
        )?;
        Ok(buffer)
    }

    /// The most recent error the pipeline posted, if it posted one.
    fn failure(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        let message = bus.pop_filtered(&[gst::MessageType::Error])?;
        let gst::MessageView::Error(error) = message.view() else {
            return None;
        };
        Some(format!("{}: {:?}", error.error(), error.debug()))
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Err(error) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!(?error, "the pipeline would not shut down");
        }
    }
}

fn make(element: &'static str) -> Result<gst::Element, Error> {
    gst::ElementFactory::make(element)
        .build()
        .map_err(|_| Error::MissingElement(element))
}

fn configure_encoder(encoder: &gst::Element) {
    // Variable bitrate rather than constant: constant pads every picture to the
    // target with filler, which was 87% of a static desktop's bitstream, and a
    // stream that is mostly padding makes the transport's bandwidth estimator
    // measure our own padding.
    encoder.set_property_from_str("rate-control", "vbr");
    // A hundred, not the sixty-six that reads as more headroom. The element
    // derives its peak as `bitrate * 100 / target-percentage` and then floors
    // the coded picture buffer at half of THAT, so sixty-six silently inflated
    // a buffer asked for as half a second into two full seconds: the log says
    // "Too small cpb_size: 4000, adjust to 24242". Two seconds of buffering is
    // two seconds of latency to spend before the rate control reacts at all.
    // Measured: at a hundred the requested buffer is honoured and no filler
    // returns.
    encoder.set_property("target-percentage", 100u32);
    // Two seconds, not the seventeen that a thousand frames comes to at sixty
    // hertz. This is the ceiling on how long a picture stays wrong: with one
    // reference frame and no intra refresh, a single frame lost on the wire
    // leaves every frame after it undecodable until the next key frame, and
    // measured on a phone that cost nine hundred frames out of two thousand
    // eight hundred. Key frames are cheap enough to spend now that parity
    // carries them and the picture is scaled to what the client shows.
    encoder.set_property("key-int-max", KEY_FRAME_INTERVAL_SECONDS * 60u32);
    encoder.set_property("b-frames", 0u32);
    encoder.set_property("ref-frames", 1u32);
    encoder.set_property("aud", true);
    // One slice: the hardware encodes the whole picture either way, and four
    // slices measured worse, 8.81 ms mean against 7.30.
    encoder.set_property("num-slices", 1u32);
    encoder.set_property_from_str("mbbrc", "disabled");
    encoder.set_property(
        "min-force-key-unit-interval",
        u64::try_from(MIN_KEYFRAME_INTERVAL.as_nanos()).unwrap_or(u64::MAX),
    );
}

/// What the scaler will import, as modifiers per DRM format.
///
/// It advertises one layout per format, the compressed one this GPU prefers,
/// and refuses a plain linear buffer, so capture has to allocate from this list
/// rather than from the compositor's.
pub(crate) fn import_layouts() -> Result<HashMap<u32, Vec<u64>>, Error> {
    gst::init()?;
    let factory =
        gst::ElementFactory::find("vapostproc").ok_or(Error::MissingElement("vapostproc"))?;
    let mut layouts: HashMap<u32, Vec<u64>> = HashMap::new();
    for template in factory.static_pad_templates() {
        if template.direction() != gst::PadDirection::Sink {
            continue;
        }
        for (structure, features) in template.caps().iter_with_features() {
            if !features.contains("memory:DMABuf") {
                continue;
            }
            let Ok(value) = structure.value("drm-format") else {
                continue;
            };
            let listed = value
                .get::<gst::List>()
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.get::<String>().ok())
                        .collect()
                })
                .or_else(|_| value.get::<String>().map(|one| vec![one]))
                .unwrap_or_default();
            for description in listed {
                if let Ok((fourcc, modifier)) = gst_video::dma_drm_fourcc_from_str(&description) {
                    layouts.entry(fourcc).or_default().push(modifier);
                }
            }
        }
    }
    Ok(layouts)
}

fn pump(
    frames: &Frames,
    control: &Control,
    counters: &Counters,
    admission: &AtomicBool,
    budget: &AtomicU32,
    output: &SyncSender<AccessUnit>,
) -> Result<(), Error> {
    let allocator = DmaBufAllocator::new();
    let mut next = 0u32;
    let mut scratch = Vec::new();
    let mut bucket = Bucket::new(Instant::now());
    let mut cadence = Cadence::default();
    let mut key_frame_bits = 0.0f64;
    while let Some(frame) = frames.next() {
        let now = Instant::now();
        // Three separate refusals, and they mean different things. The cadence
        // is the declared frame rate being honoured, which is half the
        // encoder's bitrate setting rather than a shortage. The bucket is the
        // rate limiter, refusing a picture that would cost more than the link
        // carries. The gate is a safety interlock against the transport's own
        // eviction path, and should never fire while the pacer is keeping that
        // queue shallow.
        let refusal = if !cadence.admits(control.settings().rate.framerate, now) {
            Some(&counters.paced)
        } else if !bucket.admits(budget.load(Ordering::Relaxed), key_frame_bits, now) {
            Some(&counters.refused)
        } else if !admission.load(Ordering::Relaxed) {
            Some(&counters.gated)
        } else {
            None
        };
        if let Some(counter) = refusal {
            counter.fetch_add(1, Ordering::Relaxed);
            // Dropping the frame is what returns its buffer to the ring, so a
            // refusal costs capture nothing.
            drop(frame);
            continue;
        }
        let captured = frame.captured();
        let (fd, lease) = frame.into_parts();
        let buffer = control.wrap(&allocator, fd, captured)?;

        let started = Instant::now();
        if control.source.push_buffer(buffer).is_err() {
            return Err(Error::Gst(
                control
                    .failure()
                    .unwrap_or_else(|| "the source refused a buffer".to_owned()),
            ));
        }
        // Pulling here is what makes the chain synchronous: the sample only
        // exists once the scaler has finished reading the shared buffer, so the
        // return of this call is the signal that returns it to the ring.
        let sample = control
            .sink
            .try_pull_sample(gst::ClockTime::from_nseconds(
                u64::try_from(OUTPUT_TIMEOUT.as_nanos()).unwrap_or(u64::MAX),
            ))
            .ok_or_else(|| {
                control
                    .failure()
                    .map_or(Error::EncodeTimeout(OUTPUT_TIMEOUT), Error::Gst)
            })?;
        let elapsed = started.elapsed();
        drop(lease);

        let Some(unit) = describe(&sample, next, &mut scratch, counters) else {
            continue;
        };
        bucket.charge(unit.bytes.len());
        if matches!(unit.kind, FrameKind::Key) {
            counters
                .key_frame_bytes
                .store(unit.bytes.len() as u64, Ordering::Relaxed);
            #[expect(
                clippy::cast_precision_loss,
                reason = "a frame size is far below the exact integer range of a double"
            )]
            {
                key_frame_bits = (unit.bytes.len() as f64) * 8.0;
            }
        }
        counters.record(elapsed);
        next = next.wrapping_add(1);
        if output.send(unit).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Turns one encoded sample into an access unit, dropping filler.
fn describe(
    sample: &gst::Sample,
    id: u32,
    scratch: &mut Vec<u8>,
    counters: &Counters,
) -> Option<AccessUnit> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    scratch.clear();
    scratch.reserve(map.size());
    let filler = without_filler(&map, scratch);
    counters.filler.fetch_add(filler as u64, Ordering::Relaxed);
    counters
        .bytes
        .fetch_add(scratch.len() as u64, Ordering::Relaxed);
    counters.encoded.fetch_add(1, Ordering::Relaxed);
    Some(AccessUnit {
        frame: FrameId(id),
        captured: Timestamp::from_session_micros(buffer.pts().unwrap_or_default().useconds()),
        kind: if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            FrameKind::Delta
        } else {
            FrameKind::Key
        },
        bytes: std::mem::take(scratch),
    })
}

/// Copies an Annex B access unit without its filler, returning the bytes left
/// behind.
///
/// Filler is discardable by definition, so carrying it would spend the link on
/// padding and, worse, make the transport's bandwidth estimate a measurement of
/// that padding rather than of the path.
fn without_filler(unit: &[u8], out: &mut Vec<u8>) -> usize {
    let mut discarded = 0;
    let mut cursor = 0;
    while cursor < unit.len() {
        let body = cursor + prefix_len(&unit[cursor..]);
        let end = next_prefix(&unit[body..]).map_or(unit.len(), |offset| body + offset);
        if unit.get(body).is_some_and(|byte| byte & 0x1f == NAL_FILLER) {
            discarded += end - cursor;
        } else {
            out.extend_from_slice(&unit[cursor..end]);
        }
        cursor = end;
    }
    discarded
}

/// Length of the start code the slice begins with, or zero if it begins with
/// something else.
fn prefix_len(unit: &[u8]) -> usize {
    match unit {
        [0, 0, 0, 1, ..] => 4,
        [0, 0, 1, ..] => 3,
        _ => 0,
    }
}

/// Where the next start code begins, counting a leading zero as part of it.
fn next_prefix(unit: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at + 2 < unit.len() {
        match unit[at + 2] {
            0 => at += 1,
            1 if unit[at] == 0 && unit[at + 1] == 0 => {
                return Some(at - usize::from(at > 0 && unit[at - 1] == 0));
            }
            _ => at += 3,
        }
    }
    None
}

#[derive(Debug, Default)]
struct Counters {
    encoded: AtomicU64,
    bytes: AtomicU64,
    filler: AtomicU64,
    refused: AtomicU64,
    gated: AtomicU64,
    paced: AtomicU64,
    key_frame_bytes: AtomicU64,
    latency: Mutex<Samples>,
}

impl Counters {
    fn record(&self, elapsed: Duration) {
        self.latency
            .lock()
            .expect("the latency mutex is never held across a panic")
            .push(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
    }

    fn snapshot(&self) -> EncodeStats {
        EncodeStats {
            encoded: self.encoded.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            filler_bytes: self.filler.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            gated: self.gated.load(Ordering::Relaxed),
            paced: self.paced.load(Ordering::Relaxed),
            key_frame_bytes: self.key_frame_bytes.load(Ordering::Relaxed),
            latency: self
                .latency
                .lock()
                .expect("the latency mutex is never held across a panic")
                .percentiles(),
        }
    }
}

/// A ring of the most recent timings, in microseconds.
#[derive(Debug, Default)]
struct Samples {
    held: Vec<u32>,
    next: usize,
}

impl Samples {
    const CAPACITY: usize = 4096;

    fn push(&mut self, micros: u32) {
        if self.held.len() < Self::CAPACITY {
            self.held.push(micros);
        } else {
            self.held[self.next] = micros;
            self.next = (self.next + 1) % Self::CAPACITY;
        }
    }

    fn percentiles(&self) -> Percentiles {
        if self.held.is_empty() {
            return Percentiles::default();
        }
        let mut sorted = self.held.clone();
        sorted.sort_unstable();
        let at = |share: usize| {
            Duration::from_micros(u64::from(
                sorted[(sorted.len() * share / 100).min(sorted.len() - 1)],
            ))
        };
        Percentiles {
            p50: at(50),
            p90: at(90),
            p99: at(99),
            max: Duration::from_micros(u64::from(sorted[sorted.len() - 1])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured frame sizes on this machine, at 1080p and 8 Mbit/s.
    const KEY_FRAME: usize = 130_427;
    const DELTA_FRAME: usize = 16_673;
    const TARGET: u32 = 8_000_000;
    const FRAME: Duration = Duration::from_micros(16_667);

    /// Offers `frames` pictures of `bytes` each, one frame interval apart, and
    /// reports how many the bucket admitted.
    fn offer(bucket: &mut Bucket, target: u32, bytes: usize, frames: u32) -> u32 {
        let start = Instant::now();
        let mut admitted = 0;
        for slot in 0..frames {
            let now = start + FRAME * slot;
            if bucket.admits(target, 0.0, now) {
                admitted += 1;
                bucket.charge(bytes);
            }
        }
        admitted
    }

    #[test]
    fn the_pump_delivers_at_the_rate_declared_to_the_encoder() {
        // The encoder budgets bits per frame as the bitrate divided by the rate
        // in the caps and ignores when frames actually arrive, so caps
        // declaring 25 while capture delivers 75 is three times the bitrate
        // that was asked for.
        let declared = crate::Framerate::from_millihertz(74_983 / 3).expect("nonzero");
        let mut cadence = Cadence::default();
        let start = Instant::now();
        let mut admitted = 0;
        // Three hundred capture frames at the output's own refresh.
        for slot in 0..300u32 {
            let now = start + Duration::from_nanos(u64::from(slot) * 13_336_000);
            if cadence.admits(declared, now) {
                admitted += 1;
            }
        }
        assert!(
            (98..=102).contains(&admitted),
            "a stride of three admitted {admitted} of 300"
        );
    }

    #[test]
    fn an_idle_screen_is_not_thinned_further() {
        // Capture blocks while the screen is static: frames arrive about once a
        // second. Counting every nth frame would drop two of every three of
        // those, and the screen that has just started moving would look broken.
        let declared = crate::Framerate::from_millihertz(74_983 / 3).expect("nonzero");
        let mut cadence = Cadence::default();
        let start = Instant::now();
        for slot in 0..10u32 {
            assert!(
                cadence.admits(
                    declared,
                    start + Duration::from_millis(u64::from(slot) * 900)
                ),
                "a frame a second must always be delivered"
            );
        }
    }

    #[test]
    fn a_producer_inside_the_target_is_never_refused() {
        let mut bucket = Bucket::new(Instant::now());
        // The measured delta frame at sixty frames a second is 8.00 Mbit/s, so
        // this is a producer just inside a nine megabit budget.
        assert_eq!(offer(&mut bucket, 9_000_000, DELTA_FRAME, 600), 600);
    }

    #[test]
    fn a_producer_over_the_target_is_held_to_it() {
        let mut bucket = Bucket::new(Instant::now());
        // The measurement that made this the rate limiter rather than the
        // encoder's own property: content it cannot compress produced 13.68
        // Mbit/s against 8 configured.
        let admitted = offer(&mut bucket, TARGET, DELTA_FRAME * 2, 600);
        assert!(
            (280..=320).contains(&admitted),
            "twice the target must be halved, and {admitted} of 600 got through"
        );
    }

    #[test]
    fn a_still_screen_banks_enough_to_pay_for_a_key_frame_outright() {
        let start = Instant::now();
        let mut bucket = Bucket::new(start);
        // Nothing produced for a second: the user is reading. The ceiling is a
        // key frame, not a hundred milliseconds of the target, so the picture
        // that comes back is not rationed at the moment the session becomes
        // interactive again.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame size is far below the exact integer range of a double"
        )]
        let key_frame_bits = (KEY_FRAME as f64) * 8.0;
        assert!(bucket.admits(TARGET, key_frame_bits, start + Duration::from_secs(1)));
        bucket.charge(KEY_FRAME);
        assert!(
            bucket.admits(TARGET, key_frame_bits, start + Duration::from_secs(1)),
            "a banked key frame must not leave the bucket in debt"
        );
    }

    #[test]
    fn a_refusal_is_repaid_rather_than_forgiven() {
        let start = Instant::now();
        let mut bucket = Bucket::new(start);
        // One picture far past the budget. The bucket goes negative and the
        // frames refused while it recovers are refused before the encoder ever
        // sees them, so the reference chain is intact throughout.
        assert!(bucket.admits(TARGET, 0.0, start));
        bucket.charge(KEY_FRAME);
        let mut refused = 0;
        for slot in 1..60u32 {
            if !bucket.admits(TARGET, 0.0, start + FRAME * slot) {
                refused += 1;
            }
        }
        assert!(
            (5..=12).contains(&refused),
            "a 130 kB picture is 130 ms of an eight megabit link, and this \
             refused {refused} frames of the next second"
        );
    }

    fn nal(kind: u8, len: usize) -> Vec<u8> {
        let mut unit = vec![0, 0, 0, 1, kind & 0x1f];
        unit.extend(std::iter::repeat_n(0xff, len));
        unit
    }

    #[test]
    fn filler_is_removed_and_everything_else_survives() {
        let mut stream = nal(7, 8);
        stream.extend(nal(8, 4));
        let filler = nal(NAL_FILLER, 100);
        stream.extend(filler.clone());
        stream.extend(nal(5, 32));
        let mut out = Vec::new();
        let discarded = without_filler(&stream, &mut out);
        assert_eq!(discarded, filler.len());
        assert_eq!(out.len(), stream.len() - filler.len());
        let mut expected = nal(7, 8);
        expected.extend(nal(8, 4));
        expected.extend(nal(5, 32));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_unit_with_no_filler_is_copied_unchanged() {
        let mut stream = nal(9, 1);
        stream.extend(nal(1, 64));
        let mut out = Vec::new();
        assert_eq!(without_filler(&stream, &mut out), 0);
        assert_eq!(out, stream);
    }

    #[test]
    fn three_byte_start_codes_are_understood() {
        let mut stream = vec![0, 0, 1, 9, 0x10];
        stream.extend([0, 0, 1, NAL_FILLER, 0xff, 0xff]);
        stream.extend([0, 0, 1, 1, 0x42]);
        let mut out = Vec::new();
        assert_eq!(without_filler(&stream, &mut out), 6);
        assert_eq!(out, [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 0x42]);
    }

    #[test]
    fn percentiles_hold_the_most_recent_samples() {
        let mut samples = Samples::default();
        for micros in 1..=100 {
            samples.push(micros);
        }
        let stats = samples.percentiles();
        assert_eq!(stats.p50, Duration::from_micros(51));
        assert_eq!(stats.max, Duration::from_micros(100));
    }
}
