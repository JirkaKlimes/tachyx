//! The reliable half of the connection.
//!
//! One bidirectional stream, opened by the client. Each message is a `u32`
//! big-endian length followed by that many `postcard` bytes.
//!
//! Video never shares this stream. A lost video packet must not delay an input
//! event, and an input event must not delay a frame.

use quinn::{RecvStream, SendStream};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tachyx_proto::control::{HandshakeError, Hello};

/// The largest message either end will read.
///
/// An input batch is the only message whose size depends on anything, and a
/// tick's worth of pointer motion is a few hundred bytes. Anything approaching
/// this is a peer that has lost track of the framing, and reading it would mean
/// allocating on a stranger's say-so.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// The sending half of the control stream.
#[derive(Debug)]
pub struct ControlWriter {
    stream: SendStream,
    buffer: Vec<u8>,
}

impl ControlWriter {
    /// Wraps the sending half of an opened stream.
    #[must_use]
    pub const fn new(stream: SendStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    /// Writes one message.
    ///
    /// The length and the body go out in one write, so a cancelled future
    /// cannot leave a length on the wire with no message behind it.
    ///
    /// # Errors
    /// If the message will not encode, is longer than [`MAX_MESSAGE`], or the
    /// stream has ended.
    pub async fn send<M: Serialize>(&mut self, message: &M) -> Result<(), ControlError> {
        self.buffer.clear();
        self.buffer.extend_from_slice(&[0u8; 4]);
        postcard::to_io(message, &mut self.buffer).map_err(|_| ControlError::Unencodable)?;
        let len = self.buffer.len() - 4;
        if len > MAX_MESSAGE {
            return Err(ControlError::TooLong { len });
        }
        let header = u32::try_from(len).map_err(|_| ControlError::TooLong { len })?;
        self.buffer[..4].copy_from_slice(&header.to_be_bytes());
        self.stream.write_all(&self.buffer).await?;
        Ok(())
    }

    /// Closes the stream, letting the peer see the session end cleanly.
    ///
    /// # Errors
    /// If the stream has already ended.
    pub fn finish(&mut self) -> Result<(), ControlError> {
        self.stream.finish().map_err(|_| ControlError::Closed)
    }
}

/// The receiving half of the control stream.
#[derive(Debug)]
pub struct ControlReader {
    stream: RecvStream,
    buffer: Vec<u8>,
}

impl ControlReader {
    /// Wraps the receiving half of an opened stream.
    #[must_use]
    pub const fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    /// Reads one message.
    ///
    /// # Errors
    /// If the stream ends, the peer announces a message longer than
    /// [`MAX_MESSAGE`], or the bytes are not the message expected.
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<M, ControlError> {
        let bytes = self.frame().await?;
        postcard::from_bytes(bytes).map_err(|_| ControlError::Undecodable)
    }

    /// Reads the peer's hello, which carries the protocol version.
    ///
    /// Separate from [`ControlReader::recv`] because the version is checked
    /// before the rest of the message, rather than after a decode that a
    /// version mismatch would have already turned into nonsense.
    ///
    /// # Errors
    /// If the stream ends, or the peer speaks another version of the protocol.
    pub async fn recv_hello<H: Hello>(&mut self) -> Result<H, ControlError> {
        let bytes = self.frame().await?;
        Ok(H::decode(bytes)?)
    }

    /// Reads one length-prefixed frame into the buffer.
    async fn frame(&mut self) -> Result<&[u8], ControlError> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await?;
        let len = u32::from_be_bytes(header) as usize;
        if len > MAX_MESSAGE {
            return Err(ControlError::TooLong { len });
        }
        self.buffer.clear();
        self.buffer.resize(len, 0);
        self.stream.read_exact(&mut self.buffer).await?;
        Ok(&self.buffer)
    }
}

/// Why the control stream stopped carrying messages.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The peer speaks another version of the protocol, or sent no hello.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    /// The stream ended, or the peer reset it.
    #[error("the control stream ended")]
    Ended(#[from] quinn::ReadExactError),
    /// The stream would not take a write.
    #[error("the control stream would not take a write")]
    Write(#[from] quinn::WriteError),
    /// The stream is already closed.
    #[error("the control stream is closed")]
    Closed,
    /// A message that will not fit the framing.
    #[error("a control message of {len} bytes exceeds the {MAX_MESSAGE}-byte limit")]
    TooLong {
        /// The length announced or produced.
        len: usize,
    },
    /// A message that will not encode.
    #[error("a control message could not be encoded")]
    Unencodable,
    /// Bytes that are not the message expected.
    #[error("a control message could not be decoded")]
    Undecodable,
}
