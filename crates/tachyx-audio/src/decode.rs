//! Opus in, samples out, at the volume the host reported.
//!
//! One frame a call, because the device takes one frame a call. Nothing here
//! keeps a queue: what to decode next was decided in [`crate::jitter`], and the
//! only state kept is the decoder's own, which is the whole point of a codec
//! that predicts from what came before.

use opus::{Channels, Decoder};

use crate::jitter::Next;
use crate::{CHANNELS, MAX_FRAME, RATE};

/// How long a frame is when nothing has been decoded yet, in samples of one
/// channel.
///
/// Ten milliseconds, which is what the host encodes. It is only ever used before
/// the first packet arrives, because after that a concealed frame is as long as
/// the frame in front of it: the sound that was lost was recorded by the same
/// encoder as the sound around it.
const OPENING_FRAME: usize = RATE as usize / 100;

/// The Opus decoder, and where the volume ramp has got to.
#[derive(Debug)]
pub(crate) struct Voice {
    decoder: Decoder,
    /// Room for the longest frame Opus codes, so that a host changing its frame
    /// size mid-session is a longer buffer rather than an overrun.
    pcm: Vec<i16>,
    /// The length of the last frame, in samples of one channel, which is how
    /// long a concealed frame is made.
    frame: usize,
    /// The gain the last frame ended at. A frame starts where the one before it
    /// ended, which is what makes a change of volume a ramp rather than a step.
    gain: f32,
}

impl Voice {
    /// Builds a decoder for the one format this path carries.
    ///
    /// # Errors
    /// If libopus will not allocate a decoder.
    pub(crate) fn new() -> Result<Self, opus::Error> {
        Ok(Self {
            decoder: Decoder::new(RATE, Channels::Stereo)?,
            pcm: vec![0; MAX_FRAME * CHANNELS],
            frame: OPENING_FRAME,
            gain: 0.0,
        })
    }

    /// Throws away everything the decoder was predicting from.
    ///
    /// Called when the sound stops and starts again. Without it the first frame
    /// after a silence is predicted from whatever was playing before the
    /// silence, which is a fragment of the old sound at the start of the new.
    pub(crate) fn restart(&mut self) {
        let _ = self.decoder.reset_state();
        self.frame = OPENING_FRAME;
    }

    /// One frame of interleaved stereo, ready for the device.
    ///
    /// `gain` is where the volume is going; the frame ramps to it from wherever
    /// the last one ended. A step would be heard as a click, because a step in
    /// amplitude is a step in the waveform.
    pub(crate) fn render(&mut self, next: &Next, gain: f32) -> &[i16] {
        let want = self.frame * CHANNELS;
        let decoded = match next {
            Next::Packet(opus) => self.decoder.decode(opus, &mut self.pcm, false),
            // The packet behind the hole, decoded for the copy of the lost frame
            // it carries. It is not consumed: it is decoded again, as itself,
            // on the next call.
            Next::Conceal(Some(behind)) => self.decoder.decode(behind, &mut self.pcm[..want], true),
            // Nothing to decode, so libopus invents the frame from what it was
            // predicting. It fades to silence over a few frames rather than
            // cutting, which is why a hole is handed to the decoder rather than
            // filled with zeroes here.
            Next::Conceal(None) => self.decoder.decode(&[], &mut self.pcm[..want], false),
        };
        let samples = match decoded {
            Ok(frame) => {
                self.frame = frame;
                frame * CHANNELS
            }
            Err(error) => {
                // A packet the decoder will not take is one frame of silence and
                // nothing more. The stream after it is unharmed: the next packet
                // is decoded from the state this one did not change.
                tracing::debug!(%error, "an Opus packet would not decode");
                self.pcm[..want].fill(0);
                want
            }
        };
        let from = self.gain;
        self.gain = level(gain);
        amplify(&mut self.pcm[..samples], from, self.gain);
        &self.pcm[..samples]
    }
}

/// The gain a frame is played at, whatever the caller passed.
///
/// Bounded to what the volume control can mean, because the number arrives from
/// outside this crate. Above one the samples would clip, and a value that is not
/// a number becomes silence: a fault upstream should be heard as nothing rather
/// than as noise.
fn level(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Multiplies a frame by a gain that walks from `from` to `to` across it.
///
/// The whole frame is the ramp, so a volume change takes one frame to arrive:
/// ten milliseconds at the frame size the host encodes, which is slow enough
/// that no step in the waveform is audible and fast enough that the control
/// feels immediate.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the gain is never above one, so the product is inside the sample it came from"
)]
fn amplify(samples: &mut [i16], from: f32, to: f32) {
    let frames = u16::try_from(samples.len() / CHANNELS)
        .unwrap_or(u16::MAX)
        .max(1);
    let step = (to - from) / f32::from(frames);
    let mut level = from;
    for frame in samples.chunks_mut(CHANNELS) {
        for sample in frame.iter_mut() {
            *sample = (f32::from(*sample) * level) as i16;
        }
        level += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten millisecond packets of a tone, as the host's encoder would send them.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a test fixture"
    )]
    fn tone(packets: usize) -> Vec<Vec<u8>> {
        let mut encoder = opus::Encoder::new(RATE, Channels::Stereo, opus::Application::Audio)
            .expect("libopus builds an encoder");
        let mut at = 0usize;
        (0..packets)
            .map(|_| {
                let mut samples = Vec::with_capacity(OPENING_FRAME * CHANNELS);
                for _ in 0..OPENING_FRAME {
                    let seconds = at as f32 / RATE as f32;
                    let value = (16_000.0 * (seconds * 440.0 * std::f32::consts::TAU).sin()) as i16;
                    samples.extend_from_slice(&[value; CHANNELS]);
                    at += 1;
                }
                encoder
                    .encode_vec(&samples, 4_000)
                    .expect("a frame of a tone encodes")
            })
            .collect()
    }

    #[test]
    fn a_packet_comes_back_as_a_frame_of_sound() {
        let mut voice = Voice::new().expect("a decoder");
        let packets = tone(2);
        let frame = voice.render(&Next::Packet(packets[0].clone()), 1.0);
        assert_eq!(
            frame.len(),
            OPENING_FRAME * CHANNELS,
            "ten milliseconds of stereo"
        );
        assert!(
            frame.iter().any(|sample| *sample != 0),
            "a tone is not silence"
        );
    }

    #[test]
    fn a_hole_is_filled_with_a_frame_of_the_same_length() {
        let mut voice = Voice::new().expect("a decoder");
        let packets = tone(3);
        let _ = voice.render(&Next::Packet(packets[0].clone()), 1.0);
        let concealed = voice.render(&Next::Conceal(None), 1.0).len();
        assert_eq!(
            concealed,
            OPENING_FRAME * CHANNELS,
            "the device is fed the same amount whatever happened to the packet"
        );
        let repaired = voice
            .render(&Next::Conceal(Some(packets[2].clone())), 1.0)
            .len();
        assert_eq!(repaired, OPENING_FRAME * CHANNELS);
    }

    #[test]
    fn the_volume_arrives_as_a_ramp_and_not_as_a_step() {
        let mut voice = Voice::new().expect("a decoder");
        let packets = tone(2);
        let _ = voice.render(&Next::Packet(packets[0].clone()), 1.0);
        let quiet = voice.render(&Next::Packet(packets[1].clone()), 0.0);
        let first = quiet[..CHANNELS * 8].iter().map(|s| s.abs()).max();
        let last = quiet[quiet.len() - CHANNELS * 8..]
            .iter()
            .map(|sample| sample.abs())
            .max();
        assert!(
            last < first,
            "the frame has to walk down to the new volume, not jump: {first:?} to {last:?}"
        );
        assert!(
            last.unwrap_or(i16::MAX) * 20 < first.unwrap_or(0),
            "and all but arrive at it by the end of the frame: {first:?} to {last:?}"
        );
    }

    #[test]
    fn a_volume_outside_the_control_cannot_be_asked_for() {
        assert!((level(4.0) - 1.0).abs() < f32::EPSILON);
        assert!(level(-1.0).abs() < f32::EPSILON);
        assert!(level(f32::NAN).abs() < f32::EPSILON, "silence, not noise");
    }

    #[test]
    fn a_packet_the_decoder_refuses_is_one_frame_of_silence() {
        let mut voice = Voice::new().expect("a decoder");
        let frame = voice.render(&Next::Packet(vec![0xFF; 3]), 1.0);
        assert!(frame.iter().all(|sample| *sample == 0));
        assert_eq!(frame.len(), OPENING_FRAME * CHANNELS);
    }
}
