//! Writing samples into an AAudio stream.
//!
//! One call, on the client's play path. AAudio takes a bare pointer and a frame
//! count and reads whatever the stream's own format says is there, so nothing
//! but the signature below can state that the two agree.

use ndk::audio::{AudioError, AudioFormat, AudioStream};

/// Writes one block of interleaved samples, waiting up to `timeout_nanos` for
/// the device to have room.
///
/// Returns the number of frames the device took, which is fewer than it was
/// given only when the wait ran out.
///
/// # Errors
/// If the stream is not a stream of sixteen bit samples, if it says it carries
/// no channels, or if AAudio refuses the write.
pub fn write(stream: &AudioStream, samples: &[i16], timeout_nanos: i64) -> Result<u32, AudioError> {
    if stream.format() != AudioFormat::PCM_I16 {
        return Err(AudioError::InvalidFormat);
    }
    let channels = usize::try_from(stream.channel_count()).unwrap_or(0);
    if channels == 0 {
        return Err(AudioError::InvalidFormat);
    }
    let frames = i32::try_from(samples.len() / channels).unwrap_or(i32::MAX);

    #[expect(
        unsafe_code,
        reason = "the one call this module exists to make; the lint stays on so a second one is caught"
    )]
    // SAFETY: `AAudioStream_write` reads `frames * channel_count` samples of the
    // stream's own format out of the pointer. The two checks above are what make
    // that the slice: the format is the one whose sample is an `i16`, and the
    // frame count is the slice's own length divided by the channel count the
    // stream reports, so the read cannot reach past the slice whatever the
    // device claims. The slice outlives the call, which does not keep the
    // pointer.
    let written = unsafe { stream.write(samples.as_ptr().cast(), frames, timeout_nanos) };

    // `AAudioStream_write` returns the count of frames it took, and only a
    // negative return is an error. `ndk` 0.9.0 does not make that distinction:
    // its `from_result` calls every non-zero return an error, so a write that
    // took all 480 frames of a ten millisecond block comes back as
    // `Err(__Unknown(480))`. Read the count back out of it: a non-negative code
    // is the frame count the write succeeded with, and only a negative one is
    // the error AAudio actually reported.
    match written {
        Ok(frames) => Ok(frames),
        Err(AudioError::__Unknown(count)) if count >= 0 => {
            Ok(u32::try_from(count).unwrap_or(0))
        }
        Err(error) => Err(error),
    }
}
