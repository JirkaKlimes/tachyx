//! The sound pipeline: `pulsesrc ! audioconvert ! audioresample ! capsfilter
//! ! opusenc ! appsink`.
//!
//! What it records is the host's own output, tapped at the monitor source the
//! sound server exposes for a sink. The caller names that source, because which
//! sink is the default one is a question only a sound server answers and this
//! crate does not talk to one: `tachyx-hostd` asks, and hands the answer here.
//! Nothing anywhere names a device by hand.
//!
//! The tap is before the sink's own volume and mute, which is the whole reason
//! the client applies the reported volume as playback gain. So this pipeline
//! produces the same sound whatever the host's knob is set to, and muting the
//! host silences the host alone.
//!
//! One thread, and it does nothing but pull. Encoding a 10 ms frame of Opus
//! costs tens of microseconds, so the thread spends its life asleep inside
//! GStreamer, and it is a thread rather than a task because the pull is a
//! blocking call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use tachyx_proto::media::Timestamp;

use crate::encode::Idle;
use crate::{Clock, Error};

/// Bits a second the Opus encoder is asked for.
///
/// Stereo at 128 kbit/s is transparent to most listeners for music, which is
/// the hardest thing a desktop plays. It is also under a hundredth of the link
/// a session already needs for its picture, so buying quality here costs the
/// picture nothing worth measuring. The host reserves this off the video target
/// while sound is flowing rather than sending it and hoping.
pub const AUDIO_BITRATE: u32 = 128_000;

/// How much sound one Opus packet carries.
///
/// The shortest frame the encoder offers that is not wasteful: 2.5 and 5 ms
/// frames spend a larger share of every packet on the header, and 20 ms is 10
/// more milliseconds of delay on a path that already has a network on it. It is
/// also one packet per 10 ms, which is a hundred small datagrams a second
/// beside a picture that sends far more.
pub const AUDIO_FRAME: Duration = Duration::from_millis(10);

/// The elements the pipeline is built from, all of which come from a plugin
/// this environment may not have.
const ELEMENTS: [&str; 5] = [
    "pulsesrc",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "opusenc",
];

/// What the source is asked to hand over at a time, in microseconds.
///
/// One Opus frame. The source reads in these, so it bounds how long a sample
/// waits in the sound server before the encoder sees it, and asking for less
/// than a frame would only make the encoder wait for the rest of one.
const LATENCY_MICROS: i64 = 10_000;

/// How much the source may hold, in microseconds.
///
/// Four reads. Enough that a scheduling hiccup costs no sound and short enough
/// that a stall is heard as a gap rather than as everything arriving late.
const BUFFER_MICROS: i64 = 40_000;

/// Packets the sink and the channel behind it may hold.
///
/// Eighty milliseconds of sound. A consumer that falls further behind than this
/// is not going to catch up, and the oldest packets are dropped rather than
/// delivered late: sound arriving late is worse than sound not arriving, since
/// the receiver conceals a gap and cannot un-hear a delay.
const BACKLOG: u32 = 8;

/// How long the pull waits before looking at the stop flag again.
///
/// Nothing is lost by waiting: a packet that arrives sooner ends the wait. This
/// only bounds how long stopping takes.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

/// How long the pipeline is given to reach playing.
///
/// Reaching it is what proves the sound server answered and the monitor source
/// exists. A live source fails asynchronously, so a caller that only checked
/// the state change request would learn nothing.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether GStreamer has the elements the sound pipeline needs.
///
/// This is half the answer. The other half is whether a sound server is
/// running and has a default sink, which this crate has no client for and
/// `tachyx-hostd` asks with its own. A host announces sound to a client only
/// when both halves say yes.
///
/// # Errors
/// If GStreamer will not start.
pub fn audio_available() -> Result<bool, Error> {
    gst::init()?;
    Ok(ELEMENTS
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some()))
}

/// One encoded Opus packet.
#[derive(Debug, Clone)]
pub struct OpusPacket {
    /// When the sound in it was captured, on the session's own clock.
    pub captured: Timestamp,
    /// The packet itself, which is self describing: its first byte says which
    /// mode, bandwidth and frame size a decoder should use.
    pub bytes: Vec<u8>,
}

/// The sound pipeline's output.
#[derive(Debug)]
pub struct OpusPackets(Receiver<OpusPacket>);

impl OpusPackets {
    /// Waits for the next packet, or returns `None` once capture has ended.
    #[must_use]
    pub fn recv(&self) -> Option<OpusPacket> {
        self.0.recv().ok()
    }

    /// Waits for the next packet, giving up after `timeout`.
    ///
    /// # Errors
    /// [`Idle::Waiting`] when the time ran out, [`Idle::Ended`] when capture
    /// has stopped.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<OpusPacket, Idle> {
        self.0.recv_timeout(timeout).map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => Idle::Waiting,
            std::sync::mpsc::RecvTimeoutError::Disconnected => Idle::Ended,
        })
    }
}

/// A running capture and encode of the host's own sound.
///
/// Dropping this stops it. So does [`AudioCapture::stop`], which also reports
/// how the thread ended.
#[derive(Debug)]
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), Error>>>,
}

impl AudioCapture {
    /// Starts recording `device` and encoding what it hears.
    ///
    /// `device` is a monitor source of the sink whose sound is wanted, named by
    /// whoever asked the sound server which sink that is. `clock` is the
    /// session's epoch, and it must be the one the picture is stamped against
    /// or the two streams cannot be laid together at the far end. See
    /// [`Clock`].
    ///
    /// It returns only once the pipeline is really playing, so a sound server
    /// that is not there or a device that does not exist is an error from here
    /// rather than a silence later.
    ///
    /// # Errors
    /// If GStreamer lacks an element, or the pipeline will not reach playing
    /// within [`START_TIMEOUT`].
    pub fn start(device: &str, clock: Clock) -> Result<(Self, OpusPackets), Error> {
        gst::init()?;
        let (pipeline, sink) = build(device)?;
        pipeline.set_state(gst::State::Playing)?;
        let base = play(&pipeline)?;

        let (sender, receiver) = sync_channel(BACKLOG as usize);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("tachyx-audio".to_owned())
                .spawn(move || {
                    tachyx_sched::raise("sound");
                    let outcome = pump(&pipeline, &sink, clock, base, &stop, &sender);
                    // Stopped from the thread that owns it, so the pipeline is
                    // torn down however this ended and no join waits on it.
                    if let Err(error) = pipeline.set_state(gst::State::Null) {
                        tracing::warn!(?error, "the sound pipeline would not shut down");
                    }
                    outcome
                })?
        };
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            OpusPackets(receiver),
        ))
    }

    /// Stops recording and reports how the thread ended.
    ///
    /// # Errors
    /// Whatever ended the thread, unless it was this call.
    pub fn stop(mut self) -> Result<(), Error> {
        match self.end() {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(Error::Gst("the sound thread panicked".to_owned())),
            None => Ok(()),
        }
    }

    /// Asks the thread to finish, and waits for it.
    ///
    /// The wait is bounded by [`PULL_TIMEOUT`]: the thread is either inside a
    /// pull, which ends on its own, or writing a packet, which the consumer
    /// takes or refuses as soon as it drops the receiver.
    fn end(&mut self) -> Option<std::thread::Result<Result<(), Error>>> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map(JoinHandle::join)
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Some(Ok(Err(error))) = self.end() {
            tracing::warn!(%error, "sound capture ended in an error");
        }
    }
}

/// Builds the pipeline, playing nothing yet.
fn build(device: &str) -> Result<(gst::Pipeline, AppSink), Error> {
    let source = make("pulsesrc")?;
    source.set_property("device", device);
    source.set_property("latency-time", LATENCY_MICROS);
    source.set_property("buffer-time", BUFFER_MICROS);
    // The sound server's clock is not the clock the picture is stamped on. Left
    // to provide one, this source would be the pipeline's clock, every buffer's
    // timestamp would be a time on it, and the two streams would drift apart by
    // however far the sound card runs from `CLOCK_MONOTONIC`. Refusing to
    // provide one leaves the pipeline on the system clock, which is that same
    // monotonic clock, so a buffer's timestamp maps onto the session's epoch by
    // adding the base time and nothing else.
    source.set_property("provide-clock", false);

    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let filter = make("capsfilter")?;
    // The encoder accepts eight rates and Opus itself codes at 48 kHz whatever
    // it is given, so stating the rate here converts once, in the resampler,
    // rather than leaving the encoder to do it with settings nobody chose.
    filter.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 48_000i32)
            .field("channels", 2i32)
            .build(),
    );

    let encoder = make("opusenc")?;
    encoder.set_property("bitrate", i32::try_from(AUDIO_BITRATE).unwrap_or(i32::MAX));
    encoder.set_property_from_str("frame-size", "10");
    // In-band forward error correction, and the loss figure that decides how
    // much of it to carry. Both only bite while libopus is coding in its speech
    // layer, which at this rate it usually is not; they cost nothing when it is
    // not, and the receiver's own concealment is what covers the rest.
    //
    // `audio-type` is left alone deliberately. Its `restricted-lowdelay`
    // setting would save about twenty milliseconds of algorithmic delay by
    // dropping that same speech layer, which is the layer the redundancy above
    // lives in, so the two settings cannot both be had.
    encoder.set_property("inband-fec", true);
    encoder.set_property("packet-loss-percentage", 5i32);

    let sink = AppSink::builder()
        // Nothing here presents anything: a packet is wanted the moment it
        // exists, not at the time written on it.
        .sync(false)
        .max_buffers(BACKLOG)
        .drop(true)
        .build();

    let pipeline = gst::Pipeline::new();
    let elements = [
        &source,
        &convert,
        &resample,
        &filter,
        &encoder,
        sink.upcast_ref::<gst::Element>(),
    ];
    pipeline.add_many(elements)?;
    gst::Element::link_many(elements)?;
    Ok((pipeline, sink))
}

/// Waits for the pipeline to reach playing, and reports the base time every
/// buffer's timestamp is measured from.
///
/// The base time is only set on the way to playing, so this is where it can be
/// read once instead of guessed at per buffer.
fn play(pipeline: &gst::Pipeline) -> Result<u64, Error> {
    let (settled, _, _) = pipeline.state(gst::ClockTime::from_nseconds(
        u64::try_from(START_TIMEOUT.as_nanos()).unwrap_or(u64::MAX),
    ));
    settled.map_err(|_| {
        failure(pipeline).map_or_else(
            || {
                Error::Gst(format!(
                    "the sound pipeline did not start within {START_TIMEOUT:?}"
                ))
            },
            Error::Gst,
        )
    })?;
    Ok(pipeline.base_time().unwrap_or_default().nseconds())
}

/// Pulls encoded packets until told to stop, or until the pipeline ends.
fn pump(
    pipeline: &gst::Pipeline,
    sink: &AppSink,
    clock: Clock,
    base: u64,
    stop: &AtomicBool,
    out: &SyncSender<OpusPacket>,
) -> Result<(), Error> {
    let timeout =
        gst::ClockTime::from_nseconds(u64::try_from(PULL_TIMEOUT.as_nanos()).unwrap_or(u64::MAX));
    while !stop.load(Ordering::Relaxed) {
        let Some(sample) = sink.try_pull_sample(timeout) else {
            if let Some(reason) = failure(pipeline) {
                return Err(Error::Gst(reason));
            }
            if sink.is_eos() {
                return Ok(());
            }
            continue;
        };
        let Some(packet) = describe(&sample, clock, base) else {
            continue;
        };
        // A full channel blocks here, which stops the pull and lets the sink
        // drop its oldest. That is the right back pressure: the consumer is the
        // one deciding how much sound it is behind by.
        if out.send(packet).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Reads one encoded sample out as a packet stamped on the session's clock.
///
/// A buffer's timestamp is a running time, measured from the base time the
/// pipeline was given when it started playing, on a clock that is
/// `CLOCK_MONOTONIC`. Adding the two back together gives the instant the sound
/// was captured, in the units the session's epoch counts.
fn describe(sample: &gst::Sample, clock: Clock, base: u64) -> Option<OpusPacket> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    Some(OpusPacket {
        captured: buffer
            .pts()
            .map_or_else(|| clock.elapsed(), |pts| clock.stamp(base + pts.nseconds())),
        bytes: map.to_vec(),
    })
}

/// The most recent error the pipeline posted, if it posted one.
fn failure(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    let message = bus.pop_filtered(&[gst::MessageType::Error])?;
    let gst::MessageView::Error(error) = message.view() else {
        return None;
    };
    Some(format!("{}: {:?}", error.error(), error.debug()))
}

fn make(element: &'static str) -> Result<gst::Element, Error> {
    gst::ElementFactory::make(element)
        .build()
        .map_err(|_| Error::MissingElement(element))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The figures the host reserves link for, and the client sizes its jitter
    /// buffer from. Both read them from here rather than repeating them.
    #[test]
    fn one_packet_of_sound_is_ten_milliseconds() {
        assert_eq!(AUDIO_FRAME, Duration::from_millis(10));
        assert_eq!(1000 / AUDIO_FRAME.as_millis(), 100);
        assert_eq!(AUDIO_BITRATE, 128_000);
    }
}
