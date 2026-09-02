use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ndk::media_error::MediaError;
use tachyx_proto::control::{Codec, Codecs};
use tachyx_unsafe::surface::Producer;

use super::bitstream::{Coded, Unit};
use super::decoder::{Decoder, Fed};
use super::hints;
use crate::stats::{Stats, Window};

/// How many access units may wait for the decoder.
///
/// Eight is more than a tenth of a second at sixty pictures a second. A decoder
/// that far behind will not catch up by being given more to do, so the queue is
/// emptied and decoding starts again at the next key frame. Handing over the
/// backlog instead would spend decoder time on pictures that are already too old
/// to show.
const QUEUE_DEPTH: usize = 8;

/// How long the decode loop waits for an access unit before looking at the
/// decoder once more.
///
/// Nothing depends on the interval. It exists so that the loop notices a decoded
/// frame that arrived after the last look, on a screen so still that the host is
/// sending nothing at all.
const IDLE: Duration = Duration::from_millis(100);

/// How long the loop waits for a decoded frame, and only when there is no access
/// unit waiting to go in.
///
/// Longer than a frame interval at sixty a second, so a decoder that takes
/// several milliseconds still has its frame drawn on the same pass rather than
/// on the arrival of the next picture. Input never waits behind it.
const SETTLE: Duration = Duration::from_millis(20);

/// Why the present path could not be built or run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing has registered a surface producer under that identifier.
    #[error("Flutter has no surface producer {id} to draw into")]
    NoProducer {
        /// The identifier the caller offered.
        id: i64,
    },
    /// The producer exists but is offering no surface.
    #[error("surface producer {id} is offering no surface to draw into")]
    NoSurface {
        /// The identifier the caller offered.
        id: i64,
    },
    /// The device has no decoder for the codec the host encodes with.
    #[error("this device has no {0} decoder")]
    NoDecoder(Codec),
    /// No decoder would take the stream, down to the plainest configuration.
    #[error("no {codec} decoder would take a {width}x{height} stream: {source}")]
    Configure {
        /// The codec the stream is in.
        codec: Codec,
        /// The coded width offered.
        width: u16,
        /// The coded height offered.
        height: u16,
        /// What the last attempt was refused with.
        #[source]
        source: MediaError,
    },
    /// The decoder failed partway through a stream it had accepted.
    #[error("the decoder failed: {0}")]
    Codec(#[source] MediaError),
    /// One access unit is larger than the decoder's own input buffer.
    #[error("an access unit of {unit} bytes does not fit a {room} byte input buffer")]
    Oversized {
        /// How large the access unit is.
        unit: usize,
        /// How much room the decoder offered.
        room: usize,
    },
    /// The host chose a codec this client never said it could decode.
    #[error("the host is encoding {0}, which this client does not decode")]
    UnexpectedCodec(Codec),
    /// The decode thread would not start.
    #[error("the decode thread would not start")]
    Thread(#[source] std::io::Error),
}

/// The codecs this client advertises on Android.
///
/// H.264 is always in the set: every Android device is required to decode it,
/// and in practice does so in hardware. AV1 is offered only when the device
/// decodes it in hardware. Android's own software decoder exists nearly
/// everywhere, but at sixty pictures a second it costs big cores and battery
/// for a picture the hardware H.264 decoder produces at a fraction of the
/// power, so a device without the hardware is better served by the older
/// codec.
///
/// # Errors
/// Never. A device whose platform refuses the probe simply advertises H.264.
pub fn decoders() -> Result<Codecs, Error> {
    let mut set = Codecs::of(Codec::H264);
    if super::decoder::hardware(Codec::Av1) {
        set = set.with(Codec::Av1);
    }
    Ok(set)
}

/// A decoded video stream, drawn by Flutter from the decoder's own buffers.
///
/// Open the surface producer from Kotlin first and build this one on its
/// identifier. That identifier is also what a `Texture` widget draws, so
/// [`VideoTexture::id`] gives it straight back.
pub struct VideoTexture {
    id: i64,
    shared: Arc<Shared>,
    /// Taken in [`Drop`], which is the only place it is `None`.
    worker: Option<JoinHandle<()>>,
}

impl VideoTexture {
    /// Starts decoding into the surface producer registered under `producer_id`.
    ///
    /// # Errors
    /// If Kotlin has not opened that producer, or it is offering no surface.
    /// Everything the decoder itself might refuse happens on the decode thread
    /// and is reported through [`VideoTexture::failure`], because none of it can
    /// be known before the first parameter set arrives.
    pub fn new(producer_id: i64) -> Result<Self, Error> {
        let producer = tachyx_unsafe::surface::producer(producer_id)
            .ok_or(Error::NoProducer { id: producer_id })?;
        if producer.surface().is_none() {
            return Err(Error::NoSurface { id: producer_id });
        }

        let shared = Arc::new(Shared {
            start: Instant::now(),
            codec: OnceLock::new(),
            failure: OnceLock::new(),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let worker = std::thread::Builder::new()
            .name("tachyx-decode".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || Decoding::new(&shared, &producer).run()
            })
            .map_err(Error::Thread)?;

        Ok(Self {
            id: producer_id,
            shared,
            worker: Some(worker),
        })
    }

    /// Tells the decode thread which codec the host chose.
    ///
    /// Nothing else to do with it: the platform decoder is configured from the
    /// bitstream rather than from here, and it is opened when the first
    /// parameter set arrives.
    ///
    /// # Errors
    /// If it is not in [`hints::DECODES`]. The host picks from what the client
    /// advertised, so a codec that reaches here is a negotiation that went
    /// wrong rather than a device that could not keep up, and it is reported
    /// as such instead of decoding into a black rectangle.
    pub fn decode(&self, codec: Codec) -> Result<(), Error> {
        if hints::DECODES.contains(codec) {
            let _ = self.shared.codec.set(codec);
            self.shared.wake.notify_one();
            return Ok(());
        }
        let refused = Error::UnexpectedCodec(codec);
        self.shared.fail(&refused);
        Err(refused)
    }

    /// The identifier a Flutter `Texture` widget draws.
    #[must_use]
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Hands one encoded access unit to the decoder.
    ///
    /// Returns as soon as the unit is queued. The caller is the receive loop,
    /// and everything behind it, input included, waits on this call.
    ///
    /// # Errors
    /// Never. What the decoder refuses is reported by
    /// [`VideoTexture::failure`], because it happens on the decode thread long
    /// after this has returned.
    pub fn push(&self, access_unit: Vec<u8>) -> Result<(), Error> {
        let at = self.shared.start.elapsed();
        {
            let mut state = self.shared.locked();
            state.pushed += 1;
            if state.queue.len() >= QUEUE_DEPTH {
                state.superseded += u64::try_from(state.queue.len()).unwrap_or(u64::MAX);
                state.queue.clear();
                state.resyncing = true;
            }
            state.queue.push_back((access_unit, at));
        }
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Tells the decoder the stream has ended.
    pub fn finish(&self) {
        self.shared.locked().stopping = true;
        self.shared.wake.notify_one();
    }

    /// The first thing that went wrong, or why nothing is being decoded.
    ///
    /// A client showing no picture otherwise gives no reason for it, and on this
    /// path there are two reasons that are not failures and clear by themselves:
    /// no parameter set has arrived yet, and Flutter is offering no surface to
    /// draw into. Both leave every access unit on the floor, and a viewer looking
    /// at a black rectangle cannot tell either from a decoder that has crashed.
    /// A real failure outranks them and is reported for good.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.shared
            .failure
            .get()
            .cloned()
            .or_else(|| self.shared.locked().stalled.clone())
    }

    /// What the path has done so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let state = self.shared.locked();
        Stats {
            pushed: state.pushed,
            decoded: state.decoded,
            presented: state.presented,
            superseded: state.superseded,
            decode: state.decode.summary(),
            present: state.present.summary(),
        }
    }
}

impl Drop for VideoTexture {
    fn drop(&mut self) {
        self.finish();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl fmt::Debug for VideoTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VideoTexture")
            .field("id", &self.id)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Everything the pushing thread and the decode thread share.
#[derive(Debug)]
struct Shared {
    start: Instant,
    /// The codec the host chose, set once [`VideoTexture::decode`] is told.
    /// The decode thread cannot read a unit before knowing what it is written
    /// in, and the host says which before it sends a picture.
    codec: OnceLock<Codec>,
    failure: OnceLock<String>,
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Access units the decoder has not taken yet, each with the moment it was
    /// pushed.
    queue: VecDeque<(Vec<u8>, Duration)>,
    /// Set when the queue had to be emptied. Every unit is thrown away until a
    /// key frame arrives, because a picture whose references were discarded
    /// decodes to nothing at best and to a smear at worst.
    resyncing: bool,
    /// Why the last access unit was thrown away, cleared by the next frame that
    /// decodes.
    stalled: Option<String>,
    stopping: bool,
    pushed: u64,
    decoded: u64,
    presented: u64,
    superseded: u64,
    decode: Window,
    present: Window,
}

/// What the decode loop was given to do.
#[derive(Debug)]
enum Take {
    /// One access unit, and when it was pushed.
    Unit(Vec<u8>, Duration),
    /// Nothing arrived in the time allowed.
    Nothing,
    /// The stream has ended.
    Stop,
}

impl Shared {
    fn locked(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the first thing that went wrong, and only the first: what follows
    /// is almost always that one failure repeating.
    fn fail(&self, failure: &Error) {
        tracing::error!(%failure, "the decode and present path failed");
        let _ = self.failure.set(failure.to_string());
    }

    /// The next access unit, waiting up to `patience` for one.
    fn take(&self, patience: Duration) -> Take {
        let mut state = self.locked();
        if state.queue.is_empty() && !state.stopping {
            let (guard, _) = self
                .wake
                .wait_timeout(state, patience)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
        match state.queue.pop_front() {
            Some((unit, at)) => Take::Unit(unit, at),
            None if state.stopping => Take::Stop,
            None => Take::Nothing,
        }
    }

    /// Whether there is an access unit waiting, which is what decides that the
    /// loop must not stop to wait for a decoded frame.
    fn queued(&self) -> bool {
        !self.locked().queue.is_empty()
    }

    /// Whether this unit has to be thrown away because the decoder is waiting
    /// for a picture it can start from. Seeing that picture ends the wait.
    fn skipping(&self, key: bool) -> bool {
        let mut state = self.locked();
        if key {
            state.resyncing = false;
        }
        state.resyncing
    }

    /// Why the last access unit could not be used. Not a failure: both reasons
    /// clear by themselves, one at the next key frame and one when the
    /// application is in front again.
    fn stall(&self, reason: impl Into<String>) {
        self.locked().stalled = Some(reason.into());
    }

    /// One frame out of the decoder, `drawn` if it went to the screen.
    fn decoded(&self, pushed: Duration, drawn: bool) {
        let latency = self.start.elapsed().saturating_sub(pushed);
        let mut state = self.locked();
        state.stalled = None;
        state.decoded += 1;
        state.decode.record(latency);
        if drawn {
            state.presented += 1;
            state.present.record(latency);
        }
    }
}

/// What the loop did with one access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Took {
    /// The decoder has it.
    Decoding,
    /// It could not be used and is gone.
    Dropped,
    /// Every input buffer is busy. Offer it again.
    Busy,
}

/// The decode thread, and everything only it touches.
struct Decoding<'a> {
    shared: &'a Shared,
    producer: &'a Producer,
    decoder: Option<Decoder>,
    /// The coded size the newest parameter set declared.
    coded: Option<Coded>,
    /// A unit the decoder had no room for, kept until it has.
    held: Option<(Vec<u8>, Duration)>,
    /// Access units the decoder has not yet answered for.
    in_flight: usize,
}

impl<'a> Decoding<'a> {
    fn new(shared: &'a Shared, producer: &'a Producer) -> Self {
        Self {
            shared,
            producer,
            decoder: None,
            coded: None,
            held: None,
            in_flight: 0,
        }
    }

    fn run(mut self) {
        loop {
            match self.turn() {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    self.shared.fail(&error);
                    return;
                }
            }
        }
    }

    /// One pass: take an access unit if there is one, give it to the decoder,
    /// and draw whatever the decoder has finished. `false` ends the thread.
    fn turn(&mut self) -> Result<bool, Error> {
        if self.held.is_none() {
            match self.shared.take(IDLE) {
                Take::Unit(unit, at) => self.held = Some((unit, at)),
                Take::Nothing => {}
                Take::Stop => return Ok(false),
            }
        }
        if let Some((unit, at)) = self.held.take() {
            match self.offer(&unit, at)? {
                Took::Busy => self.held = Some((unit, at)),
                Took::Decoding => self.in_flight += 1,
                Took::Dropped => {}
            }
        }
        self.draw()?;
        Ok(true)
    }

    /// Gives one access unit to a decoder that is right for it.
    fn offer(&mut self, unit: &[u8], at: Duration) -> Result<Took, Error> {
        // The host says which codec it encodes before it sends a picture, so
        // by the time there is a unit to read there is a codec to read it as.
        let Some(codec) = self.shared.codec.get().copied() else {
            self.shared
                .stall("the host has not said which codec the stream is in");
            return Ok(Took::Dropped);
        };
        let read = Unit::of(codec, unit);
        if let Some(coded) = read.coded {
            self.coded = Some(coded);
        }
        if self.shared.skipping(read.key) {
            self.shared
                .stall("the decoder is waiting for a key frame to start again from");
            return Ok(Took::Dropped);
        }
        // Nothing has said how large the pictures are, which is the one thing
        // the decoder cannot be started without. The next key frame carries it,
        // because the host repeats the parameter sets before every one.
        let Some(size) = self.coded else {
            self.shared
                .stall("no parameter set has arrived, so the picture size is unknown");
            return Ok(Took::Dropped);
        };
        // A decoder that will not start is not the end of the path. The surface
        // it is offered is replaced whenever the producer is resized, and one
        // that has just been abandoned refuses every codec until the next
        // arrives. So this is said rather than raised, and the next key frame
        // tries again.
        if let Err(refused) = self.prepare(codec, size) {
            self.shared.stall(refused.to_string());
            return Ok(Took::Dropped);
        }
        let Some(decoder) = self.decoder.as_mut() else {
            self.shared
                .stall("Flutter is offering no surface for the decoder to draw into");
            return Ok(Took::Dropped);
        };
        Ok(match decoder.feed(unit, at)? {
            Fed::Taken => Took::Decoding,
            Fed::Busy => Took::Busy,
        })
    }

    /// Makes sure the decoder in hand is the one this stream and this surface
    /// need.
    ///
    /// Both change while a session runs. The size changes when the host changes
    /// resolution, and the surface changes whenever the producer is resized or
    /// the application comes back from the background. A new size means a new
    /// decoder, which costs nothing because the unit that carries a new
    /// parameter set is a key frame. A new surface does not: pointing the
    /// running decoder at it keeps every reference picture, and the alternative
    /// is a still picture until the next key frame, which on a stream that is
    /// losing nothing can be minutes away.
    fn prepare(&mut self, codec: Codec, size: Coded) -> Result<(), Error> {
        // Flutter has taken the surface back, which it does while the
        // application is not in front. The decoder keeps running into the
        // surface it already holds and its frames are thrown away undrawn, so
        // that the picture is there the moment the surface is.
        let Some(surface) = self.producer.surface() else {
            return Ok(());
        };
        if self
            .decoder
            .as_ref()
            .is_none_or(|decoder| decoder.coded() != size)
        {
            self.decoder = Some(Decoder::open(codec, &surface, size)?);
            self.in_flight = 0;
            return Ok(());
        }
        if let Some(decoder) = self.decoder.as_mut()
            && decoder.generation() != surface.generation()
        {
            decoder.draw_into(&surface)?;
        }
        Ok(())
    }

    /// Draws everything the decoder has ready.
    fn draw(&mut self) -> Result<(), Error> {
        let Some(decoder) = self.decoder.as_ref() else {
            return Ok(());
        };
        let drawn = self.producer.surface().is_some();
        // Waiting for a frame is only ever done with no access unit waiting to
        // go in, so input never queues behind output.
        let mut patience = if self.in_flight > 0 && !self.shared.queued() {
            SETTLE
        } else {
            Duration::ZERO
        };
        while let Some(pushed) = decoder.drain(patience, drawn)? {
            self.in_flight = self.in_flight.saturating_sub(1);
            self.shared.decoded(pushed, drawn);
            patience = Duration::ZERO;
        }
        Ok(())
    }
}
