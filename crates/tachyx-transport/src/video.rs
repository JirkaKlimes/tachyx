//! Video over QUIC datagrams, and the gate that keeps frames whole.
//!
//! A successful call to [`quinn::Connection::send_datagram`] does not mean the
//! datagram was sent. Under pressure quinn discards queued datagrams to make
//! room for the new one, oldest first, and reports nothing: in a burst test
//! 2166 of 5000 offered datagrams vanished with no error. Those discards fall
//! wherever the queue happens to hold them, so what they destroy is not a frame
//! but pieces of several frames.
//!
//! Worse than that, and measured here rather than read: the discard path in
//! quinn-proto 0.11.17 subtracts a dropped datagram's length from its own
//! accounting twice, once in `DatagramBuffer::pop_front` and again in the loop
//! that called it. The second subtraction underflows, so reaching that path is
//! not a lossy degradation. It panics the connection task where overflow checks
//! are on, and wraps the byte count to something near `usize::MAX` where they
//! are not, which makes every later report of free space zero.
//!
//! The whole of this module exists to make that path unreachable. A frame is
//! offered only when the queue provably has room for all of it, and is
//! otherwise refused whole, by us, before it is sent.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::Connection;
use tachyx_proto::control::CodedSize;
use tachyx_proto::media::{FrameKind, HEADER_LEN};
use tachyx_proto::packet::{AccessUnit, Frame, Packetiser, Reassembler, ReassemblyStats};

/// Bytes of accounting quinn charges for each queued datagram, on top of its
/// payload.
///
/// Its outgoing queue holds `quinn_proto::frame::Datagram`, which is one
/// `Bytes` and nothing else, and charges `size_of` that per entry. This is the
/// term [`quinn::Connection::datagram_send_buffer_space`] leaves out: that
/// report subtracts one of these from the whole buffer, while the eviction loop
/// it is supposed to predict charges one per datagram already queued. At the
/// ninety-two datagrams a 1080p key frame occupies, the difference is 2944
/// bytes the report does not show.
const QUEUE_OVERHEAD: usize = size_of::<Bytes>();

/// A 1080p60 key frame out of `vah264enc`, measured on the target machine.
const MEASURED_KEY_FRAME_BYTES: usize = 130_427;

/// The picture the measurement above was taken from.
const MEASURED_KEY_FRAME_PIXELS: u64 = 1920 * 1080;

/// Video bytes a datagram carries on a 1500-byte path.
///
/// Only used to size a buffer ahead of a connection. Everything on the sending
/// path takes the real figure from
/// [`quinn::Connection::max_datagram_size`] instead.
const NOMINAL_PAYLOAD: NonZeroUsize = NonZeroUsize::new(1418).unwrap();

/// Bytes of queued video the transport should be allowed to hold at `coded`.
///
/// Sizing this by "two frames" is ambiguous in a way that decides whether the
/// stream starts at all. The two frame sizes measured on the target machine are
/// 130,427 bytes in 92 datagrams for a 1080p key frame, and 16,673 bytes in 12
/// datagrams for a P frame at 8 Mbit/s and 60 Hz. Read as two P frames, "two
/// frames" is 33 KiB, into which no key frame ever fits: every key frame is
/// refused by the gate, the client never gets one to synchronise on, and no
/// picture is ever shown.
///
/// So the figure comes from a key frame, scaled by pixel count from the
/// measurement, charged the per-datagram accounting the transport applies, and
/// given half a key frame of headroom. Half rather than a whole, because a
/// second key frame is never in flight beside the first: the encoder emits one
/// and then delta frames, and a buffer deep enough for two would be a buffer
/// deep enough to hold a key frame's worth of latency.
#[must_use]
pub fn video_send_buffer(coded: CodedSize) -> usize {
    let scaled = usize::try_from(
        MEASURED_KEY_FRAME_BYTES as u64 * u64::from(coded.size().pixels())
            / MEASURED_KEY_FRAME_PIXELS,
    )
    .unwrap_or(usize::MAX);
    let datagrams = scaled.div_ceil(NOMINAL_PAYLOAD.get());
    (scaled + datagrams * (HEADER_LEN + QUEUE_OVERHEAD)) * 3 / 2
}

/// What became of a picture handed to the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// Every datagram of it was queued.
    Queued {
        /// How many datagrams it took.
        datagrams: usize,
    },
    /// Refused whole, because the queue had no room for all of it.
    Refused,
}

/// Puts encoded pictures on the wire, refusing any the transport cannot take
/// whole.
#[derive(Debug)]
pub struct VideoSender {
    connection: Connection,
    packetiser: Packetiser,
    datagrams: Vec<Vec<u8>>,
    /// The size quinn was configured to give the outgoing datagram queue.
    /// Nothing on the connection reports it back, and the gate cannot be
    /// exact without it.
    buffer: usize,
    /// Sizes of the datagrams pushed onto that queue, oldest first, less the
    /// ones quinn is known to have taken.
    outstanding: VecDeque<usize>,
    key_frame_bytes: usize,
    frames_sent: u64,
    frames_refused: u64,
}

impl VideoSender {
    /// Starts sending on `connection`, whose outgoing datagram queue holds
    /// `buffer` bytes.
    #[must_use]
    pub fn new(connection: Connection, buffer: usize) -> Self {
        Self {
            connection,
            packetiser: Packetiser::new(NOMINAL_PAYLOAD),
            datagrams: Vec::new(),
            buffer,
            outstanding: VecDeque::new(),
            key_frame_bytes: MEASURED_KEY_FRAME_BYTES,
            frames_sent: 0,
            frames_refused: 0,
        }
    }

    /// Bytes of video one datagram carries right now.
    ///
    /// Taken from the path rather than assumed, because the transport lowers it
    /// when discovery finds a smaller path and every count derived from it
    /// would then be wrong. `None` before the handshake settles, and on a peer
    /// that does not do datagrams at all.
    #[must_use]
    pub fn payload_len(&self) -> Option<NonZeroUsize> {
        NonZeroUsize::new(
            self.connection
                .max_datagram_size()?
                .saturating_sub(HEADER_LEN),
        )
    }

    /// Whether the queue has room for `datagrams` of `payload_per_datagram`
    /// bytes each without discarding anything already in it.
    ///
    /// Ask before encoding, not after. A frame that is never submitted to the
    /// encoder does not advance its reference chain, so the stream stays
    /// decodable across the gap; a frame that is encoded and then dropped
    /// leaves every later frame referring to a picture the client does not
    /// have, and the encoder's intra-refresh and long-term reference frames are
    /// both unreachable on this hardware, so nothing recovers until the next
    /// key frame.
    ///
    /// The header and the queue's own accounting are charged per datagram
    /// rather than once, for the frame being offered and for every datagram
    /// already queued. Both are terms
    /// [`quinn::Connection::datagram_send_buffer_space`] omits: it subtracts a
    /// single entry's accounting from the buffer, while the eviction loop it is
    /// meant to predict charges one per entry. The report is therefore too
    /// generous as soon as two datagrams are queued ahead of the frame, and
    /// believing it reaches an eviction that does not merely lose the frame but
    /// ends the connection. See this module's own documentation.
    #[must_use]
    pub fn can_send_frame(&self, datagrams: usize, payload_per_datagram: usize) -> bool {
        let space = self.connection.datagram_send_buffer_space();
        let Some(queued) = self.queued(space) else {
            return false;
        };
        let required = datagrams.saturating_mul(payload_per_datagram + HEADER_LEN + QUEUE_OVERHEAD);
        // `space` is the buffer less the payload bytes queued less one entry's
        // accounting, so adding that entry back gives the bytes the queue has
        // free, from which every entry in it charges its own.
        required <= (space + QUEUE_OVERHEAD).saturating_sub(queued * QUEUE_OVERHEAD)
    }

    /// Datagrams quinn's outgoing queue still holds, or `None` when the record
    /// of them cannot be squared with what quinn reports.
    ///
    /// quinn reports the payload bytes in that queue but never the number of
    /// entries holding them, and its eviction charges one
    /// [`QUEUE_OVERHEAD`] per entry. This transport is the only producer on the
    /// queue and quinn takes from the front, so what is left is the tail of
    /// what was pushed, and the tail whose sizes add up to the bytes reported
    /// is the only one it can be.
    ///
    /// When they do not add up, something removed a datagram from the middle:
    /// a path that shrinks makes quinn drop the queued datagrams too large for
    /// it. The count is then unknown, the gate refuses everything, and the
    /// record squares itself again the moment the queue drains.
    fn queued(&self, space: usize) -> Option<usize> {
        let held = self.buffer.saturating_sub(space + QUEUE_OVERHEAD);
        let mut bytes = 0;
        let mut entries = 0;
        for size in self.outstanding.iter().rev() {
            if bytes >= held {
                break;
            }
            bytes += size;
            entries += 1;
        }
        (bytes == held).then_some(entries)
    }

    /// Forgets the datagrams quinn has taken off the queue.
    fn forget_sent(&mut self) {
        let space = self.connection.datagram_send_buffer_space();
        if let Some(queued) = self.queued(space) {
            self.outstanding.drain(..self.outstanding.len() - queued);
        }
    }

    /// Whether the queue has room for a single-slice picture of `bytes`.
    ///
    /// `false` before the handshake settles, when no datagram size is known.
    #[must_use]
    pub fn can_send_bytes(&self, bytes: usize) -> bool {
        let Some(payload) = self.payload_len() else {
            return false;
        };
        self.can_send_frame(bytes.div_ceil(payload.get()), payload.get())
    }

    /// Bytes to expect the next key frame to occupy.
    ///
    /// What the encoder should put through [`VideoSender::can_send_bytes`]
    /// before it is asked for a key frame. It follows a rise at once and a fall
    /// slowly, because an estimate that is too low lets through a frame the
    /// transport then shreds, which is the failure this path exists to prevent,
    /// while one that is too high costs a frame nobody would have seen anyway.
    #[must_use]
    pub const fn key_frame_estimate(&self) -> usize {
        self.key_frame_bytes
    }

    /// Frames put on the wire.
    #[must_use]
    pub const fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// Frames refused because the queue had no room for them whole.
    #[must_use]
    pub const fn frames_refused(&self) -> u64 {
        self.frames_refused
    }

    /// The transport's own bandwidth estimate, in bits per second.
    ///
    /// The congestion controller applies to datagrams: they are
    /// acknowledgement-eliciting, gated by the congestion window, counted in
    /// flight and tracked for loss, and only never retransmitted. So this is a
    /// real measurement of the path and a delay-based estimator of our own
    /// would measure the same thing worse.
    ///
    /// `None` under any controller that does not pace, which is every
    /// controller but BBR.
    #[must_use]
    pub fn pacing_rate(&self) -> Option<u64> {
        self.connection.congestion_state().metrics().pacing_rate
    }

    /// The bitrate to aim the encoder at, in bits per second.
    ///
    /// Below the estimate, because the estimate is what the path carries with
    /// nothing to spare, and a key frame is several times the size of the
    /// frames the estimate was taken over.
    #[must_use]
    pub fn target_bitrate(&self) -> Option<u64> {
        self.pacing_rate().map(|rate| rate * 85 / 100)
    }

    /// Sends one picture, or refuses it whole.
    ///
    /// A caller that gated before encoding will rarely see
    /// [`Sent::Refused`] here: this is the second check, against the queue as
    /// it stands now rather than as it stood when the encoder was asked.
    ///
    /// # Errors
    /// If the connection has ended, or the peer will not take datagrams.
    pub fn send(&mut self, unit: &AccessUnit<'_>) -> Result<Sent, VideoError> {
        let payload = self.payload_len().ok_or(VideoError::NoDatagrams)?;
        self.packetiser.set_payload_len(payload);
        self.forget_sent();
        let datagrams = unit.datagrams(payload);
        if !self.can_send_frame(datagrams, payload.get()) {
            self.frames_refused += 1;
            return Ok(Sent::Refused);
        }
        if matches!(unit.kind(), FrameKind::Key) {
            self.key_frame_bytes = unit
                .encoded_len()
                .max(self.key_frame_bytes - self.key_frame_bytes / 16);
        }
        self.datagrams.clear();
        self.packetiser.packetise(unit, &mut self.datagrams);
        for datagram in self.datagrams.drain(..) {
            let queued = datagram.len();
            self.connection.send_datagram(Bytes::from(datagram))?;
            self.outstanding.push_back(queued);
        }
        self.frames_sent += 1;
        Ok(Sent::Queued { datagrams })
    }
}

/// Rebuilds pictures from the datagrams that arrive.
#[derive(Debug)]
pub struct VideoReceiver {
    connection: Connection,
    reassembler: Reassembler,
}

impl VideoReceiver {
    /// Starts receiving on `connection`, waiting at most `deadline` for a frame
    /// that nothing follows.
    #[must_use]
    pub fn new(connection: Connection, deadline: Duration) -> Self {
        Self {
            connection,
            reassembler: Reassembler::new(deadline),
        }
    }

    /// Whether the decoder cannot continue until a key frame arrives.
    #[must_use]
    pub const fn needs_keyframe(&self) -> bool {
        self.reassembler.needs_keyframe()
    }

    /// What this receiver has seen.
    #[must_use]
    pub const fn stats(&self) -> ReassemblyStats {
        self.reassembler.stats()
    }

    /// Waits for the next complete picture.
    ///
    /// Wakes on the earliest open frame's deadline rather than on a fixed tick,
    /// so a session watching a static screen costs no wakeups at all.
    ///
    /// # Errors
    /// If the connection ends.
    pub async fn next_frame(&mut self) -> Result<Frame, VideoError> {
        loop {
            let datagram = match self.reassembler.next_deadline() {
                Some(at) => {
                    let read = self.connection.read_datagram();
                    let at = tokio::time::Instant::from_std(at);
                    let Ok(datagram) = tokio::time::timeout_at(at, read).await else {
                        self.reassembler.expire(Instant::now());
                        continue;
                    };
                    datagram?
                }
                None => self.connection.read_datagram().await?,
            };
            match self.reassembler.push(&datagram, Instant::now()) {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(malformed) => {
                    tracing::debug!(%malformed, "discarded a datagram that is not a video packet");
                }
            }
        }
    }
}

/// Why video could not move.
#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    /// The connection ended.
    #[error("the connection ended")]
    Connection(#[from] quinn::ConnectionError),
    /// The peer will not take datagrams, or the handshake has not settled.
    #[error("the connection carries no datagrams")]
    NoDatagrams,
    /// A datagram was refused for a reason other than a full queue.
    #[error("the transport refused a datagram")]
    Refused(#[from] quinn::SendDatagramError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_buffer_holds_a_whole_key_frame_and_then_some() {
        let coded = CodedSize::new(1920, 1088).expect("a codeable size");
        let buffer = video_send_buffer(coded);
        let key_frame = MEASURED_KEY_FRAME_BYTES;
        let datagrams = key_frame.div_ceil(NOMINAL_PAYLOAD.get());
        let queued = key_frame + datagrams * (HEADER_LEN + QUEUE_OVERHEAD);
        assert!(
            buffer > queued,
            "a measured key frame takes {queued} bytes of queue and the buffer is {buffer}"
        );
        assert!(
            buffer < queued * 2,
            "a buffer deep enough for two key frames is a buffer deep enough for a \
             key frame of latency"
        );
        // The reading of "two frames" that never starts the stream.
        assert!(2 * 16_673 < queued);
    }

    #[test]
    fn the_accounting_charged_is_per_datagram() {
        // Ninety-two datagrams is what the measured 1080p key frame occupied.
        assert_eq!(QUEUE_OVERHEAD, 32);
        assert_eq!(MEASURED_KEY_FRAME_BYTES.div_ceil(NOMINAL_PAYLOAD.get()), 92);
        assert_eq!(92 * QUEUE_OVERHEAD, 2944);
    }
}
