//! The control stream: its framing, and the messages that travel on it.
//!
//! Each message is a `u32` big-endian byte length followed by that many bytes of
//! a `postcard`-encoded message. `postcard` is not self-describing, so the first
//! message carries the protocol version and a mismatch is reported rather than
//! misparsed.
//!
//! These types describe both ends of the conversation and belong beside
//! [`tachyx_proto::input`] and [`tachyx_proto::media`]. They live here while the
//! protocol crate is being extended by another author; moving them is a file move
//! and a re-export.

use serde::{Deserialize, Serialize};

/// The largest control message either peer will accept.
///
/// The length prefix arrives from the network before any of the bytes it counts,
/// so a peer that claims a gigabyte must be refused rather than allocated for.
/// The largest real message is a host hello listing every monitor.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// Length of the byte-count prefix that precedes every message.
const PREFIX: usize = 4;

/// A video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// H.264, the only codec either end handles today.
    H264,
}

/// A picture size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

/// One decoder the client can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoder {
    /// What it decodes.
    pub codec: Codec,
    /// The largest coded picture it accepts.
    pub max_size: Size,
    /// The highest frame rate it sustains at that size.
    pub max_frame_rate: u16,
}

/// Where the video is shown, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// The area the video occupies.
    pub size: Size,
    /// Physical pixels per logical pixel on the client's display.
    pub device_pixel_ratio: f32,
}

/// How the host should choose the encoded resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionMode {
    /// Encode at whatever fits the client's viewport. The usual choice: nothing
    /// is gained by sending more pixels than the client can show.
    MatchViewport,
    /// Encode at this size, whatever the viewport is. For a client that wants
    /// the host's full desktop legible while zoomed.
    Fixed(Size),
}

/// The first message on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientHello {
    /// The protocol version this client speaks.
    ///
    /// Private because it is not the caller's to choose: a client that says
    /// anything but [`tachyx_proto::VERSION`] is lying about itself.
    version: u16,
    /// Every decoder the client can drive.
    pub decoders: Vec<Decoder>,
    /// Where the video will be shown.
    pub viewport: Viewport,
    /// The resolution the user asked for.
    pub resolution: ResolutionMode,
}

impl ClientHello {
    /// Introduces a client that can drive these decoders.
    #[must_use]
    pub fn new(decoders: Vec<Decoder>, viewport: Viewport, resolution: ResolutionMode) -> Self {
        Self {
            version: tachyx_proto::VERSION,
            decoders,
            viewport,
            resolution,
        }
    }

    /// The protocol version the sender speaks.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }
}

/// One of the host's displays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    /// The connector name the host knows it by, such as `eDP-1`.
    pub name: String,
    /// Its size in physical pixels.
    pub size: Size,
    /// Its refresh rate in millihertz, which is how a compositor reports one.
    pub refresh_mhz: u32,
}

/// The host's reply to [`ClientHello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    /// The protocol version the host speaks.
    pub version: u16,
    /// Every display the host can stream.
    pub monitors: Vec<Monitor>,
}

/// What the host decided to send, and how to read it.
///
/// It carries no decoder configuration bytes. The host repeats the parameter
/// sets in the stream immediately before every key frame, so the client needs no
/// separate configuration step, survives a reconnection or a resolution change
/// without one, and recovers on its own if a parameter set is lost on a path that
/// never retransmits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamConfig {
    /// The codec in use.
    pub codec: Codec,
    /// The size the encoder codes, always a multiple of sixteen.
    pub coded: Size,
    /// The part of the coded picture that is real content.
    ///
    /// Equal to [`StreamConfig::coded`] today, because every rung of the
    /// resolution ladder is a multiple of sixteen and needs no crop. It is still
    /// carried, because a client cannot recover it from the bitstream.
    pub visible: Size,
    /// Width and height of one pixel, as the ratio the picture must be stretched
    /// by to look right. A capture scaled between two aspect ratios records the
    /// correction nowhere the bitstream can express it.
    pub pixel_aspect: (u16, u16),
    /// The frame rate declared to the encoder, which is what its bit budget is
    /// divided by.
    pub frame_rate: u16,
    /// The target bitrate in bits per second.
    pub bitrate_bps: u32,
}

/// A client-side instant, in microseconds since the session began, read from the
/// client's monotonic clock.
///
/// Taken where the event entered the client rather than where it is sent, so
/// that batching does not hide in the measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClientTime(pub u64);

/// One input event and when the client saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedInput {
    /// When the client saw it.
    pub at: ClientTime,
    /// What the user did.
    pub event: tachyx_proto::input::InputEvent,
}

/// Everything the client can say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Introduces the client. Always first.
    Hello(ClientHello),
    /// The viewport changed: a rotation, a resize, or a new resolution mode.
    /// The host treats it as renegotiation.
    ViewportChanged(Viewport),
    /// A tick's worth of input.
    ///
    /// Batched rather than sent one event at a time, so that a burst of pointer
    /// motion costs one packet instead of one per sample.
    Input(Vec<TimedInput>),
}

/// Everything the host can say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    /// Introduces the host. Always first.
    Hello(HostHello),
    /// The host's streaming decision. Re-sent whenever any of it changes,
    /// including a change forced by congestion.
    Stream(StreamConfig),
}

/// Why a control message could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// The peer announced a message larger than either end will handle.
    #[error("peer announced a {announced}-byte message; the limit is {MAX_MESSAGE}")]
    TooLarge {
        /// The length the peer claimed.
        announced: usize,
    },
    /// The bytes inside a well-formed frame were not a message.
    #[error("message body is not valid postcard")]
    Malformed,
}

/// Frames a message for the control stream: a big-endian byte count, then the
/// `postcard` encoding.
///
/// # Errors
/// If the encoded message exceeds [`MAX_MESSAGE`]. Only an implausible viewport
/// or monitor list can reach that.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, ControlError> {
    let mut out =
        postcard::to_extend(message, vec![0u8; PREFIX]).map_err(|_| ControlError::Malformed)?;
    let body = out.len() - PREFIX;
    if body > MAX_MESSAGE {
        return Err(ControlError::TooLarge { announced: body });
    }
    let count = u32::try_from(body).map_err(|_| ControlError::TooLarge { announced: body })?;
    out[..PREFIX].copy_from_slice(&count.to_be_bytes());
    Ok(out)
}

/// Cuts a byte stream into messages.
///
/// A stream delivers arbitrary slices of the bytes that were written, so a
/// message can arrive in pieces and several can arrive at once. This holds the
/// remainder until a whole message is present.
#[derive(Debug, Default)]
pub struct MessageStream {
    pending: Vec<u8>,
}

impl MessageStream {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes read from the control stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// Takes the next whole message, if one has arrived.
    ///
    /// # Errors
    /// If the peer announced a message over [`MAX_MESSAGE`], or the body of a
    /// whole frame did not decode. Both are fatal: the stream is ordered, so
    /// there is no way to resynchronise after either.
    pub fn next_message(&mut self) -> Result<Option<HostMessage>, ControlError> {
        let Some(head) = self.pending.get(..PREFIX) else {
            return Ok(None);
        };
        let announced = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if announced > MAX_MESSAGE {
            return Err(ControlError::TooLarge { announced });
        }
        let end = PREFIX + announced;
        if self.pending.len() < end {
            return Ok(None);
        }
        let message = postcard::from_bytes(&self.pending[PREFIX..end])
            .map_err(|_| ControlError::Malformed)?;
        self.pending.drain(..end);
        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> StreamConfig {
        StreamConfig {
            codec: Codec::H264,
            coded: Size {
                width: 1280,
                height: 720,
            },
            visible: Size {
                width: 1280,
                height: 720,
            },
            pixel_aspect: (1, 1),
            frame_rate: 120,
            bitrate_bps: 20_000_000,
        }
    }

    #[test]
    fn a_message_survives_the_wire() {
        let mut stream = MessageStream::new();
        stream.push(&encode(&HostMessage::Stream(config())).unwrap());
        assert_eq!(
            stream.next_message().unwrap(),
            Some(HostMessage::Stream(config()))
        );
        assert_eq!(stream.next_message().unwrap(), None);
    }

    #[test]
    fn a_message_split_across_reads_is_reassembled() {
        let bytes = encode(&HostMessage::Stream(config())).unwrap();
        let mut stream = MessageStream::new();
        for byte in &bytes[..bytes.len() - 1] {
            stream.push(std::slice::from_ref(byte));
            assert_eq!(
                stream.next_message().unwrap(),
                None,
                "a partial message is not one"
            );
        }
        stream.push(&bytes[bytes.len() - 1..]);
        assert_eq!(
            stream.next_message().unwrap(),
            Some(HostMessage::Stream(config()))
        );
    }

    #[test]
    fn several_messages_in_one_read_come_out_in_order() {
        let hello = HostMessage::Hello(HostHello {
            version: tachyx_proto::VERSION,
            monitors: Vec::new(),
        });
        let mut bytes = encode(&hello).unwrap();
        bytes.extend(encode(&HostMessage::Stream(config())).unwrap());
        let mut stream = MessageStream::new();
        stream.push(&bytes);
        assert_eq!(stream.next_message().unwrap(), Some(hello));
        assert_eq!(
            stream.next_message().unwrap(),
            Some(HostMessage::Stream(config()))
        );
        assert_eq!(stream.next_message().unwrap(), None);
    }

    #[test]
    fn an_oversized_announcement_is_refused_before_it_is_allocated_for() {
        let mut stream = MessageStream::new();
        stream.push(&u32::MAX.to_be_bytes());
        assert_eq!(
            stream.next_message(),
            Err(ControlError::TooLarge {
                announced: u32::MAX as usize
            })
        );
    }

    #[test]
    fn a_frame_that_is_not_a_message_is_refused() {
        let mut stream = MessageStream::new();
        stream.push(&3u32.to_be_bytes());
        stream.push(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(stream.next_message(), Err(ControlError::Malformed));
    }

    #[test]
    fn a_client_cannot_claim_a_version_it_does_not_speak() {
        let hello = ClientHello::new(
            Vec::new(),
            Viewport {
                size: Size {
                    width: 1080,
                    height: 2400,
                },
                device_pixel_ratio: 2.75,
            },
            ResolutionMode::MatchViewport,
        );
        assert_eq!(hello.version(), tachyx_proto::VERSION);
    }
}
