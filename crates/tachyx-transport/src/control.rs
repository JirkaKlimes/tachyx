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
}

/// Bytes of length prefix in front of each message.
const HEADER: usize = 4;

/// The receiving half of the control stream.
///
/// [`ControlReader::recv`] is cancel-safe, which is what lets it sit in a
/// `select!` beside a timer. That is not free and it is not incidental: a
/// message is a length and then the body it announces, quinn's `read_exact` is
/// documented as *not* cancel-safe, and a read cancelled between the two would
/// take the bytes it had already consumed with it. The next length would then be
/// read out of the middle of a message and the framing would never square again.
/// The connection survives that, because keepalives and datagrams are on other
/// paths, so the symptom is not a disconnection but a control stream that goes
/// silent for the rest of the session.
///
/// So bytes come off the stream a chunk at a time, into a buffer that outlives
/// any one call, and framing is done over that buffer rather than over the
/// stream.
#[derive(Debug)]
pub struct ControlReader {
    stream: RecvStream,
    /// Bytes taken off the stream and not yet framed, including any tail of the
    /// next message that arrived with this one.
    pending: Vec<u8>,
    /// The body of the message last framed.
    message: Vec<u8>,
}

impl ControlReader {
    /// Wraps the receiving half of an opened stream.
    #[must_use]
    pub const fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            message: Vec::new(),
        }
    }

    /// Reads one message.
    ///
    /// Cancel-safe: a dropped future leaves the stream where it found it. See
    /// [`ControlReader`].
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

    /// Takes the next whole frame out of the buffer, reading more when the
    /// buffer does not hold one yet.
    async fn frame(&mut self) -> Result<&[u8], ControlError> {
        loop {
            if let Some(len) = self.announced()?
                && self.pending.len() >= HEADER + len
            {
                self.message.clear();
                self.message
                    .extend_from_slice(&self.pending[HEADER..HEADER + len]);
                self.pending.drain(..HEADER + len);
                return Ok(&self.message);
            }
            // The one await, and the only place bytes leave the stream.
            // `read_chunk` is cancel-safe: it reports data only once it has
            // taken it, so a future dropped here has consumed nothing.
            let chunk = self
                .stream
                .read_chunk(MAX_MESSAGE, true)
                .await?
                .ok_or(ControlError::Ended)?;
            self.pending.extend_from_slice(&chunk.bytes);
        }
    }

    /// The body length the buffered header announces, once there is one.
    ///
    /// # Errors
    /// If the peer announces a message longer than [`MAX_MESSAGE`]. Reading it
    /// would mean allocating on a stranger's say-so.
    fn announced(&self) -> Result<Option<usize>, ControlError> {
        let Some(header) = self.pending.get(..HEADER) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(header.try_into().expect("four bytes")) as usize;
        if len > MAX_MESSAGE {
            return Err(ControlError::TooLong { len });
        }
        Ok(Some(len))
    }
}

/// Why the control stream stopped carrying messages.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The peer speaks another version of the protocol, or sent no hello.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    /// The peer finished the stream, part way through a message or between two.
    #[error("the control stream ended")]
    Ended,
    /// The stream would not take a read.
    #[error("the control stream would not take a read")]
    Read(#[from] quinn::ReadError),
    /// The stream would not take a write.
    #[error("the control stream would not take a write")]
    Write(#[from] quinn::WriteError),
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
