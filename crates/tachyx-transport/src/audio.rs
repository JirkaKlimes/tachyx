//! Audio over the same QUIC datagrams the picture travels on.
//!
//! It shares the connection, the sequence numbers and the congestion
//! accounting with [`crate::video`], and it deliberately shares nothing else.
//!
//! Above all it does not share the pacer. A picture is released at a byte rate
//! and a key frame takes tens of milliseconds to leave, so an Opus packet
//! queued behind one arrives tens of milliseconds late, every key frame, for as
//! long as the session lasts. Sound is the one thing in the session that cannot
//! absorb that: a gap in a picture is a frame nobody sees and a gap in sound is
//! a click everybody hears. So an audio datagram is handed to the connection
//! the moment it is encoded.
//!
//! What that costs is ordering: two threads hand datagrams over and the
//! sequence numbers they stamp can reach the wire the other way round. That
//! costs nothing at all. The receiver report carries signed arrival deltas
//! precisely because reordering on the path is ordinary, and a controller that
//! could not read a reordered pair could not read a Wi-Fi retry either.
//!
//! One packet at a time, and no gate ahead of the encoder. The gate in
//! [`crate::video`] exists because a whole frame has to fit or be refused
//! whole, and a picture refused after it is encoded costs a key frame; an Opus
//! packet is a few hundred bytes and there is nothing to refuse it in favour
//! of. What sound still shares is the interlock in [`crate::queue`], which
//! refuses any datagram the queue has no room for. Nothing may reach quinn's
//! eviction path, and a few hundred bytes reach it as surely as a key frame
//! does.

use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Instant;

use tachyx_congestion::Release;
use tachyx_proto::media::{FrameId, HEADER_LEN, Seq, Timestamp};
use tachyx_proto::packet::{Sequence, audio_datagram};

use crate::queue::{Handed, Outgoing};

/// Puts encoded sound on the wire, beside the picture rather than behind it.
///
/// Cloneable and cheap to clone: everything in it is a handle. It is made by
/// [`crate::video::VideoSender::audio`], which is what ties the two paths to
/// one connection, one sequence and one set of release records.
#[derive(Debug, Clone)]
pub struct AudioSender {
    /// The connection, and the record of what is queued on it, both shared
    /// with the picture path. See [`crate::queue`].
    outgoing: Arc<Outgoing>,
    /// The counter the picture path stamps from. See
    /// [`tachyx_proto::packet::Sequence`].
    sequence: Arc<Sequence>,
    /// Where the record of each release goes, which is the same channel the
    /// picture path writes to and the congestion controller drains.
    records: SyncSender<Release>,
}

impl AudioSender {
    /// Builds a sender on `connection`, stamping from `sequence` and recording
    /// into `records`.
    pub(crate) const fn new(
        outgoing: Arc<Outgoing>,
        sequence: Arc<Sequence>,
        records: SyncSender<Release>,
    ) -> Self {
        Self {
            outgoing,
            sequence,
            records,
        }
    }

    /// Sends one Opus packet, captured at `capture` and numbered `id`.
    ///
    /// `id` counts the session's audio packets and must skip none: the hole in
    /// the numbers is the only thing that tells a receiver how much sound to
    /// conceal.
    ///
    /// It does not block. Nothing waits, nothing is paced, and the call returns
    /// as soon as the connection has taken the datagram.
    ///
    /// # Errors
    /// If the connection has ended, the peer will not take datagrams, the
    /// packet is one no datagram can carry, or the outgoing queue has no room
    /// for it.
    pub fn send(&self, id: FrameId, capture: Timestamp, opus: &[u8]) -> Result<(), AudioError> {
        let max = self
            .outgoing
            .connection()
            .max_datagram_size()
            .ok_or(AudioError::NoDatagrams)?;
        // Every refusal is decided before a sequence number is spent, and that
        // order is the point. A number stamped on a datagram that never leaves
        // is a hole the receiver reads as a lost packet, and it reads it for
        // the rest of the session.
        if opus.is_empty() {
            return Err(AudioError::Empty);
        }
        let wire = HEADER_LEN + opus.len();
        if wire > max {
            return Err(AudioError::TooLarge);
        }
        let at = Instant::now();
        let mut stamped = None;
        let handed = self.outgoing.send(wire, || {
            let seq = self.sequence.take();
            stamped = Some(seq);
            audio_datagram(seq, id, capture, opus)
        })?;
        match (handed, stamped) {
            (Handed::Queued, Some(seq)) => {
                self.record(seq, at, wire);
                Ok(())
            }
            // Nothing was built, so no number was spent and the sequence has no
            // hole in it. The packet itself is gone: the receiver reads the gap
            // in the audio numbering and conceals the sound that was in it.
            _ => Err(AudioError::NoRoom),
        }
    }

    /// Records one release for the congestion controller.
    ///
    /// Into the same channel the picture path writes to, and it has to be: the
    /// controller matches a receiver report against these records by sequence
    /// number, and a datagram it has no record of is a datagram it skips. Audio
    /// that went unrecorded would be a tenth of the session's datagrams missing
    /// from every delay measurement taken.
    ///
    /// Never part of a probe cluster. A cluster measures the path by releasing
    /// datagrams back to back above the pacing rate, and these leave whenever
    /// the encoder produces them.
    fn record(&self, seq: Seq, at: Instant, wire: usize) {
        let record = Release {
            seq,
            at,
            wire: u16::try_from(wire).unwrap_or(u16::MAX),
            cluster: None,
        };
        // Dropped rather than blocking the send path, exactly as the picture
        // path drops one. The controller skips a datagram it has no record of.
        if let Err(TrySendError::Full(_)) = self.records.try_send(record) {
            tracing::debug!("an audio release record was dropped");
        }
    }
}

/// Why sound could not move.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The peer will not take datagrams, or the handshake has not settled.
    #[error("the connection carries no datagrams")]
    NoDatagrams,
    /// The packet has no bytes, which a receiver reads as a malformed datagram
    /// rather than as silence. An encoder in discontinuous transmission
    /// produces these, and what they mean is that nothing is to be sent.
    #[error("an audio packet of no bytes cannot be sent")]
    Empty,
    /// The packet does not fit the path.
    #[error("an audio packet is larger than the path carries")]
    TooLarge,
    /// The outgoing queue had no room, so the packet was dropped rather than
    /// offered.
    ///
    /// Offering it would evict whatever the queue already holds, which on this
    /// version of quinn ends the connection. The caller counts the packet as
    /// lost and numbers the next one as though it had been sent: the gap is
    /// what tells the receiver to conceal.
    #[error("the outgoing queue had no room for an audio packet")]
    NoRoom,
    /// The connection ended, or refused the datagram for some other reason.
    #[error("the transport refused a datagram")]
    Refused(#[from] quinn::SendDatagramError),
}
