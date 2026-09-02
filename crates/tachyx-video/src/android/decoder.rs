use std::time::Duration;

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use tachyx_unsafe::surface::Surface;

use super::Error;
use super::bitstream::Coded;
use super::hints::{LOW_LATENCY, MIME};

/// Whether the decoder took the access unit it was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fed {
    /// It is in the decoder.
    Taken,
    /// Every input buffer is still busy. Offer the same unit again.
    Busy,
}

/// The platform decoder, configured for one coded size and one surface.
///
/// Nothing decoded ever passes through this process. The decoder is given the
/// window behind Flutter's surface producer and writes its own buffers into it;
/// [`Decoder::drain`] does no more than say which of them the compositor should
/// draw.
#[derive(Debug)]
pub(super) struct Decoder {
    codec: MediaCodec,
    coded: Coded,
    generation: u64,
    stamp: u64,
}

impl Decoder {
    /// Starts a decoder for a `coded` stream, drawing into `surface`.
    ///
    /// No codec configuration is supplied out of band. The host repeats the
    /// parameter sets before every key frame, so a decoder set up from the
    /// bitstream alone survives a reconnection and a resolution change without
    /// anything being said to it.
    ///
    /// # Errors
    /// If the device has no H.264 decoder, or none would take the stream even
    /// with every latency request dropped.
    pub(super) fn open(surface: &Surface, coded: Coded) -> Result<Self, Error> {
        let mut refused = Error::NoDecoder;
        for keys in LOW_LATENCY {
            let decoder = MediaCodec::from_decoder_type(MIME).ok_or(Error::NoDecoder)?;
            let mut format = MediaFormat::new();
            format.set_str("mime", MIME);
            format.set_i32("width", i32::from(coded.width));
            format.set_i32("height", i32::from(coded.height));
            for key in keys {
                format.set_i32(key, 1);
            }
            match decoder
                .configure(
                    &format,
                    Some(surface.window()),
                    MediaCodecDirection::Decoder,
                )
                .and_then(|()| decoder.start())
            {
                Ok(()) => {
                    return Ok(Self {
                        codec: decoder,
                        coded,
                        generation: surface.generation(),
                        stamp: 0,
                    });
                }
                Err(source) => {
                    refused = Error::Configure {
                        width: coded.width,
                        height: coded.height,
                        source,
                    };
                }
            }
        }
        Err(refused)
    }

    /// The size this decoder writes.
    pub(super) fn coded(&self) -> Coded {
        self.coded
    }

    /// Which of the producer's surfaces it writes into.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Points the decoder at a surface that has replaced the one it was built
    /// with.
    ///
    /// Everything decoded so far is kept, which is the whole reason this is not
    /// a rebuild: a rebuilt decoder shows nothing until the next key frame, and
    /// key frames are minutes apart on a stream that is not losing packets.
    ///
    /// # Errors
    /// If the decoder will not take the surface.
    pub(super) fn draw_into(&mut self, surface: &Surface) -> Result<(), Error> {
        self.codec
            .set_output_surface(surface.window())
            .map_err(Error::Codec)?;
        self.generation = surface.generation();
        Ok(())
    }

    /// Hands one whole access unit over, stamped with the moment it was pushed.
    ///
    /// The stamp is the measurement: it comes back on the decoded frame, so the
    /// age of a frame is read off the frame itself and no table maps frames back
    /// to pushes. `MediaCodec` requires the stamps to increase, so two units
    /// pushed within the same microsecond are recorded a microsecond apart.
    ///
    /// # Errors
    /// If the decoder fails, or the unit is larger than one of its buffers.
    pub(super) fn feed(&mut self, unit: &[u8], at: Duration) -> Result<Fed, Error> {
        let DequeuedInputBufferResult::Buffer(mut buffer) = self
            .codec
            .dequeue_input_buffer(Duration::ZERO)
            .map_err(Error::Codec)?
        else {
            return Ok(Fed::Busy);
        };

        let room = buffer.buffer_mut();
        if room.len() < unit.len() {
            return Err(Error::Oversized {
                unit: unit.len(),
                room: room.len(),
            });
        }
        for (slot, byte) in room.iter_mut().zip(unit) {
            slot.write(*byte);
        }

        let pushed = u64::try_from(at.as_micros()).unwrap_or(u64::MAX);
        let stamp = pushed.max(self.stamp);
        self.stamp = stamp.saturating_add(1);
        self.codec
            .queue_input_buffer(buffer, 0, unit.len(), stamp, 0)
            .map_err(Error::Codec)?;
        Ok(Fed::Taken)
    }

    /// Takes one decoded frame, if `wait` is long enough for one to appear.
    ///
    /// `render` draws it. There is nothing to do afterwards: the surface
    /// producer schedules the frame itself, and pacing it here would only put
    /// this client's idea of the display's rhythm in front of the compositor's.
    ///
    /// Returns the moment the access unit behind the frame was pushed.
    ///
    /// # Errors
    /// If the decoder fails.
    pub(super) fn drain(&self, wait: Duration, render: bool) -> Result<Option<Duration>, Error> {
        let DequeuedOutputBufferInfoResult::Buffer(frame) = self
            .codec
            .dequeue_output_buffer(wait)
            .map_err(Error::Codec)?
        else {
            return Ok(None);
        };
        let pushed = u64::try_from(frame.info().presentation_time_us()).unwrap_or(0);
        self.codec
            .release_output_buffer(frame, render)
            .map_err(Error::Codec)?;
        Ok(Some(Duration::from_micros(pushed)))
    }
}
