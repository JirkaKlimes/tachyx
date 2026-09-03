//! The sound output on a desktop: GStreamer into the machine's sound server.
//!
//! The pipeline is deliberately short. Samples reach it already at 48 kHz in
//! stereo, which is what the sound server wants, so the two converters in the
//! middle negotiate to nothing on every machine measured and are there for the
//! one that wants something else.

use gstreamer::prelude::*;
use gstreamer_app::AppSrc;

/// How much sound the source holds before a push waits, in bytes.
///
/// Four frames of stereo at 48 kHz, which is under eight kilobytes. It is the
/// only thing that makes a write block, and blocking is what paces the decoder
/// against the speaker, so it must be small enough to be a real back pressure
/// and large enough that a scheduler pause does not starve the sink.
const SOURCE_BYTES: u64 = 4 * 480 * 2 * 2;

/// What the sound server is asked for, in microseconds.
///
/// Forty milliseconds of device buffer, filled ten at a time. Below this the
/// machine has to wake this thread a hundred times a second and any pause longer
/// than a frame is heard; far above it the buffer is latency added to every
/// packet.
const BUFFER_MICROS: i64 = 40_000;

/// How much the sink asks for at a time, in microseconds.
const LATENCY_MICROS: i64 = 10_000;

/// The machine's sound output, as a running pipeline.
#[derive(Debug)]
pub(crate) struct Speaker {
    pipeline: gstreamer::Pipeline,
    source: AppSrc,
}

/// Opens the machine's sound output.
///
/// # Errors
/// If GStreamer will not start, the elements are missing, or the pipeline will
/// not play, which on this path means no sound server answered.
pub(crate) fn open() -> Result<Speaker, String> {
    gstreamer::init().map_err(|error| format!("GStreamer would not start: {error}"))?;
    let pipeline = gstreamer::parse::launch(&description())
        .map_err(|error| format!("the play pipeline would not build: {error}"))?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| "a parsed pipeline description is a pipeline".to_owned())?;
    let source = pipeline
        .by_name("source")
        .and_then(|element| element.downcast::<AppSrc>().ok())
        .ok_or_else(|| "the description names an appsrc".to_owned())?;
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|_| "the play pipeline would not start".to_owned())?;
    Ok(Speaker { pipeline, source })
}

/// The pipeline, from pushed samples to the speaker.
///
/// `sync=false` on the sink is what makes the device's own buffer the pacer. The
/// samples carry no useful presentation time: they are played the moment there
/// is room for them, and the buffer above decides which sound that is. A sink
/// synchronising instead would compare a timestamp taken at the push against a
/// clock it runs itself and throw away everything it thought was late.
fn description() -> String {
    let sink = if gstreamer::ElementFactory::find("pulsesink").is_some() {
        format!("pulsesink sync=false buffer-time={BUFFER_MICROS} latency-time={LATENCY_MICROS}")
    } else {
        // Whatever the machine has. It is the answer for a desktop with no sound
        // server of the usual kind, and it takes none of the settings above.
        "autoaudiosink sync=false".to_owned()
    };
    format!(
        "appsrc name=source is-live=true format=time do-timestamp=true block=true \
         max-bytes={SOURCE_BYTES} \
         caps=audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
         ! audioconvert \
         ! audioresample \
         ! {sink}"
    )
}

impl Speaker {
    /// Hands the device one frame of interleaved stereo, waiting for room.
    ///
    /// # Errors
    /// If the pipeline has stopped taking samples, which is the sound server
    /// having gone away.
    pub(crate) fn write(&mut self, frame: &[i16]) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(frame.len() * 2);
        for sample in frame {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.source
            .push_buffer(gstreamer::Buffer::from_mut_slice(bytes))
            .map(|_| ())
            .map_err(|flow| format!("the sound output stopped taking samples: {flow}"))
    }

    /// Tells the pipeline no more sound is coming.
    pub(crate) fn finish(&mut self) {
        let _ = self.source.end_of_stream();
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}
