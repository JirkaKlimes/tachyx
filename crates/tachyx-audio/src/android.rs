//! The sound output on a phone: AAudio, written from the play thread.
//!
//! No callback. AAudio offers one, and it runs on a real time thread that may
//! not take a lock, so a player built on it needs a lock free ring between the
//! decoder and the callback and a second buffer's worth of latency to feed it.
//! A blocking write costs neither: it returns when the device has room, which is
//! the same pacing the callback would have given, on a thread that is allowed to
//! decode.
//!
//! The stream belongs to the thread that opened it, which is why it is opened
//! where it is used.

use ndk::audio::{
    AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder,
};

use crate::{CHANNELS, RATE};

/// How long a write waits for room before it gives up, in nanoseconds.
///
/// A second, which is a hundred frames of sound. Nothing that is working comes
/// near it: a write waits for one frame. It is there so that a device which has
/// stopped consuming ends the session's sound instead of parking the thread
/// forever.
const WRITE_TIMEOUT: i64 = 1_000_000_000;

/// The machine's sound output, as an open stream.
#[derive(Debug)]
pub(crate) struct Speaker {
    stream: AudioStream,
}

/// Opens the phone's sound output and starts it.
///
/// # Errors
/// If AAudio will not give out a stream, which is the device being held
/// exclusively by something else or the format being refused.
pub(crate) fn open() -> Result<Speaker, String> {
    let channels = i32::try_from(CHANNELS).unwrap_or(2);
    let stream = AudioStreamBuilder::new()
        .map_err(|error| format!("AAudio would not start: {error}"))?
        .direction(AudioDirection::Output)
        .format(AudioFormat::PCM_I16)
        .channel_count(channels)
        .sample_rate(i32::try_from(RATE).unwrap_or(48_000))
        // Shared rather than exclusive: an exclusive stream is refused whenever
        // anything else is playing, and a session that went silent because a
        // notification chimed would be worse than a millisecond of mixing.
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .open_stream()
        .map_err(|error| format!("AAudio would not open an output: {error}"))?;
    stream
        .request_start()
        .map_err(|error| format!("AAudio would not start the output: {error}"))?;
    Ok(Speaker { stream })
}

impl Speaker {
    /// Hands the device one frame of interleaved stereo, waiting for room.
    ///
    /// # Errors
    /// If the stream is not taking samples of this shape any more, which is a
    /// device that was unplugged or a stream the system took away.
    pub(crate) fn write(&mut self, frame: &[i16]) -> Result<(), String> {
        tachyx_unsafe::aaudio::write(&self.stream, frame, WRITE_TIMEOUT)
            .map(|_| ())
            .map_err(|error| format!("the sound output stopped taking samples: {error}"))
    }

    /// Stops the stream. Dropping it closes the device.
    pub(crate) fn finish(&mut self) {
        let _ = self.stream.request_stop();
    }
}
