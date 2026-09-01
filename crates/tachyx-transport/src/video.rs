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
//!
//! The datagrams of a frame that is accepted are then released at a byte rate
//! rather than handed over back to back. Why, and at what rate, is in
//! [`crate::pace`].

use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::Connection;
use tachyx_congestion::{ClusterId, Probe, Release, cluster_datagrams};
use tachyx_proto::control::CodedSize;
use tachyx_proto::media::{FrameKind, HEADER_LEN, Seq};
use tachyx_proto::packet::{AccessUnit, Packetiser, Parity};

use crate::pace::{Pacer, rate_for};

/// Release records held for the controller to read.
///
/// Three seconds of history at 20 Mbit/s is 5,289 datagrams, and the controller
/// drains this every report and again on its own tick, so this is generous. A
/// record that does not fit is dropped rather than blocking the send path, and
/// the controller skips a datagram it has no record of, which is exactly what
/// it does for one that has aged out.
const RECORD_BACKLOG: usize = 8_192;

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

/// An ordinary 1080p60 picture at 8 Mbit/s, measured on the same machine.
const MEASURED_DELTA_FRAME_BYTES: usize = 16_673;

/// The picture the measurement above was taken from.
const MEASURED_KEY_FRAME_PIXELS: u64 = 1920 * 1080;

/// Video bytes a datagram carries on a 1500-byte path.
///
/// Only used to size a buffer ahead of a connection. Everything on the sending
/// path takes the real figure from
/// [`quinn::Connection::max_datagram_size`] instead.
const NOMINAL_PAYLOAD: NonZeroUsize = NonZeroUsize::new(1418).unwrap();

/// The longest the send path sleeps without looking at the connection.
///
/// Only reached at rates where one datagram is worth more than this, which is
/// under about 6 Mbit/s. Everywhere above it a datagram's own wait is shorter
/// and the sleep is the whole of it.
const WAIT_SLICE: Duration = Duration::from_millis(2);

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
///
/// The key frame's parity is counted with it. It goes on this same queue and
/// the gate must be able to admit the frame whole, repair included.
#[must_use]
pub fn video_send_buffer(coded: CodedSize) -> usize {
    let scaled = usize::try_from(
        MEASURED_KEY_FRAME_BYTES as u64 * u64::from(coded.size().pixels())
            / MEASURED_KEY_FRAME_PIXELS,
    )
    .unwrap_or(usize::MAX);
    let data = scaled.div_ceil(NOMINAL_PAYLOAD.get());
    let parity = Parity::fixed(Parity::CEILING).shards(FrameKind::Key, data);
    let queued =
        scaled + parity * NOMINAL_PAYLOAD.get() + (data + parity) * (HEADER_LEN + QUEUE_OVERHEAD);
    queued * 3 / 2
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
    /// Bytes to expect an ordinary picture to occupy.
    ///
    /// Tracked apart from the key frame, because asking for key frame room
    /// before an ordinary picture asks for roughly eight times what is needed
    /// and refuses almost everything. Measured on a phone: 30 frames reached
    /// the wire out of 130 encoded, and the session died having shown none.
    delta_bytes: usize,
    /// What the session is configured to send, in bits a second. The rate a
    /// picture is released at is derived from it and from that picture's own
    /// size on the wire.
    ///
    /// Shared rather than owned, because this lives inside a blocking task for
    /// the life of the session and the controller that moves it does not. Same
    /// shape as `tachyx_media::Encoder::admission`, and for the same reason.
    target: Arc<AtomicU32>,
    pacer: Pacer,
    /// Where the record of each release goes, for the controller to match a
    /// receiver report against.
    records: SyncSender<Release>,
    /// Clusters the controller has asked for.
    probes: Receiver<Probe>,
    /// The parity level, as a percentage of a frame's picture datagrams.
    parity: Arc<AtomicU8>,
    armed: Option<Armed>,
    frames_sent: u64,
    frames_refused: u64,
    padding_sent: u64,
}

/// A probe cluster part way through going out.
#[derive(Debug, Clone, Copy)]
struct Armed {
    cluster: ClusterId,
    rate: NonZeroU32,
    left: usize,
}

/// What the congestion controller drives the send path through.
///
/// The path itself runs in a blocking task, because releasing a paced key frame
/// takes tens of milliseconds of waiting, and the controller runs beside the
/// rest of the session. Everything they have to say to each other crosses here:
/// the rate to release at, the clusters to release above it, and the record of
/// what actually went out and when.
#[derive(Debug)]
pub struct VideoControl {
    target: Arc<AtomicU32>,
    parity: Arc<AtomicU8>,
    records: Receiver<Release>,
    probes: SyncSender<Probe>,
}

impl VideoControl {
    /// Sets the rate the pacer releases against, in bits a second.
    ///
    /// The transport does not decide this and cannot: it is the session's
    /// operating point. What the transport does with it is release datagrams at
    /// a multiple of it, so that pacing never becomes the bottleneck. See
    /// [`crate::pace::rate_for`].
    pub fn set_target_bitrate(&self, target: NonZeroU32) {
        self.target.store(target.get(), Ordering::Relaxed);
    }

    /// Sets how much parity every frame carries, as a percentage of its own
    /// picture datagrams.
    ///
    /// The level follows measured loss, which the receiver report is the only
    /// source of. Sunshine and Moonlight carry a flat fifth for ever; this one
    /// rises on the first report that asks for it and falls only once the lower
    /// reading has held, because too little parity costs a broken reference
    /// chain and a key frame while too much costs a few percent of the link.
    pub fn set_parity(&self, percent: u8) {
        self.parity.store(percent, Ordering::Relaxed);
    }

    /// Asks for a cluster to be released above the pacing rate.
    ///
    /// It goes out as the next picture if one is ready, which costs nothing at
    /// all because the same bytes were leaving anyway, and as padding if none
    /// is. Padding is the case that matters: a still desktop produces no frames
    /// for seconds at a time, which is precisely when a probe is most needed.
    ///
    /// A probe offered while one is still going out is dropped: one cluster
    /// measures the link and two overlapping ones measure each other.
    pub fn probe(&self, probe: Probe) {
        match self.probes.try_send(probe) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                tracing::debug!("the video path ended before a probe reached it");
            }
        }
    }

    /// Every datagram released since this was last called.
    pub fn released(&self) -> impl Iterator<Item = Release> + '_ {
        self.records.try_iter()
    }
}

impl VideoSender {
    /// Starts sending on `connection`, whose outgoing datagram queue holds
    /// `buffer` bytes, for a session configured to send `target` bits a second.
    #[must_use]
    pub fn new(connection: Connection, buffer: usize, target: NonZeroU32) -> (Self, VideoControl) {
        let shared = Arc::new(AtomicU32::new(target.get()));
        let parity = Arc::new(AtomicU8::new(Parity::MEASURED));
        let (records, drained) = sync_channel(RECORD_BACKLOG);
        // One deep. A second cluster offered while the first is still going out
        // would measure the first rather than the link.
        let (asked, probes) = sync_channel(1);
        (
            Self {
                connection,
                packetiser: Packetiser::new(NOMINAL_PAYLOAD),
                datagrams: Vec::new(),
                buffer,
                outstanding: VecDeque::new(),
                key_frame_bytes: MEASURED_KEY_FRAME_BYTES,
                delta_bytes: MEASURED_DELTA_FRAME_BYTES,
                target: Arc::clone(&shared),
                pacer: Pacer::new(rate_for(target, MEASURED_KEY_FRAME_BYTES), Instant::now()),
                records,
                probes,
                parity: Arc::clone(&parity),
                armed: None,
                frames_sent: 0,
                frames_refused: 0,
                padding_sent: 0,
            },
            VideoControl {
                target: shared,
                parity,
                records: drained,
                probes: asked,
            },
        )
    }

    /// The rate the session is configured to send at, in bits a second.
    fn target(&self) -> NonZeroU32 {
        NonZeroU32::new(self.target.load(Ordering::Relaxed)).unwrap_or(NonZeroU32::MIN)
    }

    /// The rate the last picture was released at, in bits a second.
    ///
    /// Not one rate for the session. Every picture is paced at what its own
    /// size on the wire asks for, so a key frame is released faster than a
    /// delta frame in order to reach the receiver inside the same deadline.
    #[must_use]
    pub const fn release_rate(&self) -> NonZeroU32 {
        self.pacer.rate()
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
        // Charged its parity as well, at the key frame rate, because the caller
        // asking this is the one about to ask for a key frame.
        let data = bytes.div_ceil(payload.get());
        let parity = self.packetiser.parity().shards(FrameKind::Key, data);
        self.can_send_frame(data + parity, payload.get())
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

    /// Bytes to expect the next ordinary picture to occupy.
    ///
    /// This, not the key frame, is what a caller should gate on. Almost every
    /// picture is an ordinary one, and demanding room for the rare large one
    /// before each of them refuses the stream into silence. A key frame that
    /// does not fit is refused when it arrives, which costs a repair rather
    /// than the whole picture.
    #[must_use]
    pub const fn delta_estimate(&self) -> usize {
        self.delta_bytes
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

    /// Datagrams of padding released to time the path.
    #[must_use]
    pub const fn padding_sent(&self) -> u64 {
        self.padding_sent
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
    /// **This blocks** until the last datagram of the picture has been released
    /// at the pacing rate, which for a key frame is tens of milliseconds. It is
    /// meant to be called from a thread of its own, as
    /// `tachyx_hostd::session`'s pump does. Nothing that produces frames is
    /// held while it waits: capture writes into a slot that displaces rather
    /// than queues, and the encoder's own output channel is bounded, so a slow
    /// consumer here costs dropped capture frames and never a stalled capture.
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
        self.packetiser
            .set_parity(Parity::fixed(self.parity.load(Ordering::Relaxed)));
        self.take_probe(payload);
        self.forget_sent();
        // The packetiser's count, not the picture's: parity is real bytes on
        // this same queue, and a gate that counted only the picture would
        // under-estimate by a fifth and reach the eviction path above.
        let datagrams = self.packetiser.datagrams(unit);
        if !self.can_send_frame(datagrams, payload.get()) {
            self.frames_refused += 1;
            return Ok(Sent::Refused);
        }
        if matches!(unit.kind(), FrameKind::Key) {
            self.key_frame_bytes = unit
                .encoded_len()
                .max(self.key_frame_bytes - self.key_frame_bytes / 16);
        } else {
            self.delta_bytes = unit
                .encoded_len()
                .max(self.delta_bytes - self.delta_bytes / 16);
        }
        // Taken out of `self` for the release loop, which needs the rest of
        // `self` for the pacer, the queue record and the connection, and put
        // back after it so the allocation outlives the frame.
        let mut queue = std::mem::take(&mut self.datagrams);
        queue.clear();
        self.packetiser.packetise(unit, &mut queue);
        // Paced at the rate this picture's own weight on the wire asks for,
        // parity and per-datagram headers included. Measured against the
        // encoded length instead, a key frame carrying parity is understated by
        // about a third and leaves over half as long again as intended.
        //
        // Unless a cluster is going out, in which case the picture *is* the
        // cluster: the same bytes were leaving anyway and releasing them faster
        // measures the link for nothing.
        let weight = queue.iter().map(Vec::len).sum();
        let paced = rate_for(self.target(), weight);
        // Never below what the picture's own weight asks for: a key frame paced
        // slower than the receiver's deadline is a key frame thrown away, and a
        // probe rate is not an excuse to miss it.
        self.pacer
            .set_rate(self.armed.map_or(paced, |probe| probe.rate.max(paced)));
        let sent = self.release(&mut queue, datagrams);
        queue.clear();
        self.datagrams = queue;
        // Counted here rather than in the release loop, because padding goes
        // through the same loop and is not a picture.
        if matches!(sent, Ok(Sent::Queued { .. })) {
            self.frames_sent += 1;
        }
        // The picture was the head of a cluster and did not fill it, so the
        // tail follows in the same burst rather than behind the next frame.
        self.finish_cluster(payload)?;
        sent
    }

    /// Puts a packetised picture on the wire at the pacing rate.
    ///
    /// `datagrams` is what the caller reports the picture as having taken,
    /// which is not the length of `queue`: the queue also carries whatever
    /// parity the packetiser added.
    fn release(&mut self, queue: &mut Vec<Vec<u8>>, datagrams: usize) -> Result<Sent, VideoError> {
        for datagram in queue.drain(..) {
            let queued = datagram.len();
            let seq = seq_of(&datagram);
            let wait = self.pacer.release(queued, Instant::now());
            if !wait.is_zero() {
                self.wait(wait)?;
                // The queue drained while this frame was being released, so the
                // record of what quinn still holds is stale by now. Squaring it
                // is what keeps the gate exact for the next frame.
                self.forget_sent();
            }
            let at = Instant::now();
            match self.connection.send_datagram(Bytes::from(datagram)) {
                Ok(()) => {
                    self.outstanding.push_back(queued);
                    self.record(seq, at, queued);
                }
                // The path's measured size can shrink underneath a frame: the
                // transport lowers it from its own task when it decides a larger
                // packet is being black-holed, and that lands mid-frame during
                // the first seconds of a session. Refusing the rest of this
                // picture is right, and the fragments already queued are
                // harmless because a receiver abandons a frame it cannot
                // complete. Treating it as fatal would end video for the life of
                // the session over a transient the transport is in the middle of
                // correcting.
                Err(quinn::SendDatagramError::TooLarge) => {
                    self.frames_refused += 1;
                    return Ok(Sent::Refused);
                }
                Err(other) => return Err(other.into()),
            }
        }
        Ok(Sent::Queued { datagrams })
    }

    /// Records one release for the controller, and counts it against whatever
    /// cluster is going out.
    ///
    /// The time is taken after the datagram was handed over rather than before
    /// the pacer's wait, because what the delay estimator needs is when the
    /// datagram actually left. A send time taken at queue entry is a queue
    /// entry time, and a delay estimate built on wrong send times is worse than
    /// none.
    fn record(&mut self, seq: Option<Seq>, at: Instant, wire: usize) {
        let cluster = self.armed.and_then(|probe| {
            let armed = self.armed.as_mut()?;
            armed.left -= 1;
            let cluster = probe.cluster;
            if armed.left == 0 {
                self.armed = None;
            }
            Some(cluster)
        });
        let Some(seq) = seq else { return };
        let record = Release {
            seq,
            at,
            wire: u16::try_from(wire).unwrap_or(u16::MAX),
            cluster,
        };
        // A record that does not fit is dropped rather than blocking the send
        // path. The controller skips a datagram it has no record of, which is
        // what it does for one that has aged out.
        if let Err(TrySendError::Full(_)) = self.records.try_send(record) {
            tracing::debug!("a release record was dropped");
        }
    }

    /// Arms whatever cluster the controller has asked for.
    fn take_probe(&mut self, payload: NonZeroUsize) {
        if self.armed.is_some() {
            return;
        }
        if let Ok(probe) = self.probes.try_recv() {
            self.armed = Some(Armed {
                cluster: probe.cluster,
                rate: probe.rate,
                left: cluster_datagrams(probe.rate, payload.get() + HEADER_LEN),
            });
        }
    }

    /// Finishes an armed cluster with padding.
    ///
    /// A cluster has to leave back to back or it measures nothing: a cluster
    /// that straddles two frames spans the idle interval between them, and both
    /// its send spacing and its arrival spacing are then a measurement of the
    /// encoder rather than of the path. Measured on a shaped 12 Mbit/s link, a
    /// cluster spanning a frame boundary read 5.2 Mbit/s and dragged the
    /// estimate down with it.
    ///
    /// So a picture is the head of its cluster and padding is the tail, and the
    /// two go out in one burst. Padding is also the whole of a cluster when no
    /// picture came along to be one, which on a desktop nobody is touching is
    /// the ordinary case: a still screen produces no frames at all, and a
    /// controller that only ever learns from media never learns that the link
    /// has recovered.
    ///
    /// # Errors
    /// If the connection has ended, or the peer will not take datagrams.
    fn finish_cluster(&mut self, payload: NonZeroUsize) -> Result<usize, VideoError> {
        let Some(probe) = self.armed else {
            return Ok(0);
        };
        self.forget_sent();
        if !self.can_send_frame(probe.left, payload.get()) {
            self.armed = None;
            return Ok(0);
        }
        let mut queue = std::mem::take(&mut self.datagrams);
        queue.clear();
        for _ in 0..probe.left {
            queue.push(self.packetiser.padding(payload.get()));
        }
        let offered = queue.len();
        let released = self.release(&mut queue, offered);
        queue.clear();
        self.datagrams = queue;
        match released? {
            Sent::Queued { datagrams } => {
                self.padding_sent += datagrams as u64;
                Ok(datagrams)
            }
            Sent::Refused => Ok(0),
        }
    }

    /// Releases a cluster of padding, when no picture came along to be one.
    ///
    /// Called on the send path's idle tick. See [`VideoSender::finish_cluster`].
    ///
    /// # Errors
    /// If the connection has ended, or the peer will not take datagrams.
    pub fn pad(&mut self) -> Result<usize, VideoError> {
        let payload = self.payload_len().ok_or(VideoError::NoDatagrams)?;
        self.packetiser.set_payload_len(payload);
        self.take_probe(payload);
        if let Some(probe) = self.armed {
            self.pacer.set_rate(probe.rate);
        }
        self.finish_cluster(payload)
    }

    /// Sleeps out a datagram's turn, in slices short enough that a connection
    /// which ends is noticed rather than slept through.
    ///
    /// A frame paced at a low rate is tens of milliseconds of waiting, and a
    /// client that disconnects in the middle of one should end the send rather
    /// than have the rest of the picture pushed at a connection that is gone.
    fn wait(&self, wait: Duration) -> Result<(), VideoError> {
        let due = Instant::now() + wait;
        while let Some(left) = due.checked_duration_since(Instant::now()) {
            if let Some(reason) = self.connection.close_reason() {
                return Err(VideoError::Connection(reason));
            }
            std::thread::sleep(left.min(WAIT_SLICE));
        }
        Ok(())
    }
}

/// The sequence number a built datagram carries.
///
/// Read back out of the bytes rather than threaded alongside them, because the
/// packetiser is what stamps it and there is one place it appears. Sixteen bits
/// at a fixed offset is two loads.
fn seq_of(datagram: &[u8]) -> Option<Seq> {
    let bytes = datagram.get(8..10)?;
    Some(Seq(u16::from_le_bytes([bytes[0], bytes[1]])))
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
        let data = key_frame.div_ceil(NOMINAL_PAYLOAD.get());
        // Charged at the ceiling rather than at the level a session opens on,
        // because the level rises with loss and a key frame that no longer fits
        // is a key frame the gate refuses for ever.
        let parity = Parity::fixed(Parity::CEILING).shards(FrameKind::Key, data);
        let queued = key_frame
            + parity * NOMINAL_PAYLOAD.get()
            + (data + parity) * (HEADER_LEN + QUEUE_OVERHEAD);
        assert!(
            buffer > queued,
            "a measured key frame and its parity take {queued} bytes of queue and the \
             buffer is {buffer}"
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
