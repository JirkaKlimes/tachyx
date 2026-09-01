//! Cutting the control stream into messages.
//!
//! Each message is a `u32` big-endian byte length followed by that many bytes of
//! a `postcard`-encoded message from [`tachyx_proto::control`]. QUIC delivers a
//! stream, not messages, so a message can arrive in pieces and several can
//! arrive at once.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest control message this client will accept.
///
/// The length prefix arrives before the bytes it counts, so a peer that claims a
/// gigabyte must be refused rather than allocated for. The largest real message
/// is a stream configuration, which is tens of bytes.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// Length of the byte-count prefix that precedes every message.
const PREFIX: usize = 4;

/// Why a control message could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    /// The peer announced a message larger than this client will handle.
    #[error("peer announced a {announced}-byte message; the limit is {MAX_MESSAGE}")]
    TooLarge {
        /// The length the peer claimed.
        announced: usize,
    },
    /// The bytes inside a well-formed frame were not the message they claim.
    #[error("message body is not valid postcard")]
    Malformed,
}

/// Frames a message for the control stream.
///
/// # Errors
/// If the encoded message exceeds [`MAX_MESSAGE`].
pub fn frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FramingError> {
    let mut out =
        postcard::to_extend(message, vec![0u8; PREFIX]).map_err(|_| FramingError::Malformed)?;
    let body = out.len() - PREFIX;
    let count = u32::try_from(body)
        .ok()
        .filter(|_| body <= MAX_MESSAGE)
        .ok_or(FramingError::TooLarge { announced: body })?;
    out[..PREFIX].copy_from_slice(&count.to_be_bytes());
    Ok(out)
}

/// Holds the remainder of a partly delivered message until the rest arrives.
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
    /// there is nothing left to resynchronise to.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FramingError> {
        let Some(head) = self.pending.get(..PREFIX) else {
            return Ok(None);
        };
        let announced = usize::try_from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
            .map_err(|_| FramingError::Malformed)?;
        if announced > MAX_MESSAGE {
            return Err(FramingError::TooLarge { announced });
        }
        let end = PREFIX + announced;
        if self.pending.len() < end {
            return Ok(None);
        }
        let message = postcard::from_bytes(&self.pending[PREFIX..end])
            .map_err(|_| FramingError::Malformed)?;
        self.pending.drain(..end);
        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU16, NonZeroU32};

    use tachyx_proto::control::{Aspect, Codec, CodedSize, HostMessage, Size, StreamConfig};

    use super::*;

    fn stream() -> HostMessage {
        HostMessage::Stream(StreamConfig {
            codec: Codec::H264,
            coded: CodedSize::new(1280, 720).unwrap(),
            visible: Size::new(1280, 720).unwrap(),
            display_aspect: Aspect::new(16, 9).unwrap(),
            framerate: NonZeroU16::new(120).unwrap(),
            bitrate: NonZeroU32::new(20_000_000).unwrap(),
        })
    }

    #[test]
    fn a_message_survives_the_wire() {
        let mut incoming = MessageStream::new();
        incoming.push(&frame(&stream()).unwrap());
        assert_eq!(incoming.next_message(), Ok(Some(stream())));
        assert_eq!(incoming.next_message::<HostMessage>(), Ok(None));
    }

    #[test]
    fn a_message_split_across_reads_is_reassembled() {
        let bytes = frame(&stream()).unwrap();
        let mut incoming = MessageStream::new();
        for byte in &bytes[..bytes.len() - 1] {
            incoming.push(std::slice::from_ref(byte));
            assert_eq!(
                incoming.next_message::<HostMessage>(),
                Ok(None),
                "a partial message is not one"
            );
        }
        incoming.push(&bytes[bytes.len() - 1..]);
        assert_eq!(incoming.next_message(), Ok(Some(stream())));
    }

    #[test]
    fn several_messages_in_one_read_come_out_in_order() {
        let mut bytes = frame(&stream()).unwrap();
        bytes.extend(frame(&stream()).unwrap());
        let mut incoming = MessageStream::new();
        incoming.push(&bytes);
        assert_eq!(incoming.next_message(), Ok(Some(stream())));
        assert_eq!(incoming.next_message(), Ok(Some(stream())));
        assert_eq!(incoming.next_message::<HostMessage>(), Ok(None));
    }

    #[test]
    fn an_oversized_announcement_is_refused_before_it_is_allocated_for() {
        let mut incoming = MessageStream::new();
        incoming.push(&u32::MAX.to_be_bytes());
        assert_eq!(
            incoming.next_message::<HostMessage>(),
            Err(FramingError::TooLarge {
                announced: u32::MAX as usize
            })
        );
    }

    #[test]
    fn a_frame_that_is_not_a_message_is_refused() {
        let mut incoming = MessageStream::new();
        incoming.push(&3u32.to_be_bytes());
        incoming.push(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(
            incoming.next_message::<HostMessage>(),
            Err(FramingError::Malformed)
        );
    }
}
