//! Access units to datagrams, and datagrams back to access units.
//!
//! Both directions live here rather than in the transport because they are pure
//! functions over the sixteen-byte header of [`crate::media`], and because the
//! client reassembles video without wanting a QUIC server linked in to do it.
//!
//! Parity lives here too, for the same reason and one more: it is the sender's
//! choice of how many shards to spend and the receiver's arithmetic to spend
//! them, and putting the two anywhere but next to each other is how the two
//! halves of an erasure code drift apart.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::num::{NonZeroU16, NonZeroUsize};
use std::time::{Duration, Instant};

use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

use crate::media::{
    Fragment, FrameId, FrameKind, HEADER_LEN, MalformedPacket, PacketHeader, ParityHeader,
    PictureHeader, Role, Seq, SliceId, Timestamp,
};

/// Slices one access unit may hold.
///
/// Identifier 255 is reserved for forward error correction, so the highest a
/// picture may use is 254.
pub const MAX_SLICES: usize = 255;

/// One encoded picture, ready for the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnit<'a> {
    id: FrameId,
    capture: Timestamp,
    kind: FrameKind,
    slices: &'a [&'a [u8]],
}

impl<'a> AccessUnit<'a> {
    /// Describes one encoded picture.
    ///
    /// `slices` are the picture's independently decodable pieces, in decode
    /// order. A parameter set belongs to the piece carrying the picture it
    /// configures, never to a piece of its own: giving SPS and PPS a slice to
    /// themselves would let the wire lose them while the key frame still
    /// arrived complete, leaving the client holding an undecodable key frame
    /// and no reason to ask for another.
    ///
    /// # Errors
    /// If the picture has no slices, more slices than [`MAX_SLICES`], or a
    /// slice with no bytes.
    pub fn new(
        id: FrameId,
        capture: Timestamp,
        kind: FrameKind,
        slices: &'a [&'a [u8]],
    ) -> Result<Self, MalformedUnit> {
        if slices.is_empty() {
            return Err(MalformedUnit::Empty);
        }
        if slices.len() > MAX_SLICES {
            return Err(MalformedUnit::TooManySlices);
        }
        if slices.iter().any(|slice| slice.is_empty()) {
            return Err(MalformedUnit::EmptySlice);
        }
        Ok(Self {
            id,
            capture,
            kind,
            slices,
        })
    }

    /// Which frame this is.
    #[must_use]
    pub const fn id(&self) -> FrameId {
        self.id
    }

    /// When the host captured it.
    #[must_use]
    pub const fn capture(&self) -> Timestamp {
        self.capture
    }

    /// Whether it stands alone.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The picture's slices, in decode order.
    #[must_use]
    pub const fn slices(&self) -> &'a [&'a [u8]] {
        self.slices
    }

    /// Encoded bytes across every slice.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.slices.iter().map(|slice| slice.len()).sum()
    }

    /// Datagrams of picture this occupies at `payload_len` bytes each, before
    /// any parity.
    ///
    /// Fragmentation is per slice, so this is not `encoded_len` divided by
    /// `payload_len` once a picture has more than one slice. A caller sizing a
    /// send queue wants [`Packetiser::datagrams`] instead, which adds the
    /// parity the picture will actually be sent with.
    #[must_use]
    pub fn datagrams(&self, payload_len: NonZeroUsize) -> usize {
        self.slices
            .iter()
            .map(|slice| slice.len().div_ceil(payload_len.get()))
            .sum()
    }
}

/// Why a picture cannot be packetised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MalformedUnit {
    /// No slices at all.
    #[error("an access unit with no slices carries no picture")]
    Empty,
    /// More slices than the one-byte identifier can name.
    #[error("an access unit holds at most {MAX_SLICES} slices")]
    TooManySlices,
    /// A slice with no bytes, which would produce a datagram that is all header
    /// and which a receiver rejects as [`MalformedPacket::NoPayload`].
    #[error("a slice with no bytes cannot be sent")]
    EmptySlice,
}

/// How much parity a frame carries, as a percentage of its picture datagrams.
///
/// A percentage rather than a fixed count because the thing that decides
/// whether a frame survives is how many of its own datagrams it can afford to
/// lose, and that scales with how many it has. At the 2.79% loss measured on
/// the wireless link in `spec/findings.md`, an unprotected 92-datagram key
/// frame arrives intact with probability 0.074 and 20% parity takes it to
/// 1.0000, for **less** total bandwidth than the key-frame-on-demand recovery
/// it replaces: a recovery key frame costs eight frames and parity costs a
/// fifth of one.
///
/// There is no receiver report on the control stream yet, so nothing feeds
/// [`Parity::observe`] in a running session. Until there is, a session runs at
/// [`Parity::MEASURED`], which is the level that measurement was taken at, and
/// the congestion work can drive the level from real feedback by calling
/// [`Packetiser::set_parity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parity {
    level: u8,
    /// Consecutive observations asking for less than `level`. Loss is not
    /// symmetric: too little parity costs a broken reference chain and a key
    /// frame, too much costs a few percent of the link, so the level rises on
    /// the first report that asks for it and falls only once the lower reading
    /// has held.
    falling: u8,
}

impl Parity {
    /// The least parity a frame carries once parity is on at all.
    ///
    /// Loss is measured after the fact, so a link that has just started
    /// dropping packets is measured as clean. This is what covers the reports
    /// between the first loss and the level that answers it.
    pub const FLOOR: u8 = 5;

    /// The most parity a frame carries.
    ///
    /// Above this the repair is most of what is being sent, and a link that
    /// loses a third of its packets is better served by a lower bitrate than by
    /// spending the difference on parity.
    pub const CEILING: u8 = 50;

    /// The level the Monte Carlo in `spec/findings.md` was run at.
    pub const MEASURED: u8 = 20;

    /// How far below the applied level a report must ask before it counts
    /// towards a fall, in percentage points.
    const SLACK: u8 = 3;

    /// Consecutive reports below the applied level before it falls. At the 50
    /// ms report interval specified in `spec/congestion.md` this is 200 ms.
    const HOLD: u8 = 4;

    /// A session that carries `percent` of parity on every delta frame, and
    /// proportionally more on every key frame.
    #[must_use]
    pub const fn fixed(percent: u8) -> Self {
        Self {
            level: if percent > Self::CEILING {
                Self::CEILING
            } else {
                percent
            },
            falling: 0,
        }
    }

    /// The percentage applied to a delta frame.
    #[must_use]
    pub const fn level(self) -> u8 {
        self.level
    }

    /// The percentage applied to a frame of this kind.
    ///
    /// A key frame is protected harder than a delta frame, and not because it
    /// is larger: the percentage already sizes the parity against the datagram
    /// count. It is because the two failures cost different things. A delta
    /// frame that dies costs one key frame request. A key frame that dies costs
    /// the request *and* the answer, and the answer is the frame most likely to
    /// die next, which is the bistable failure `spec/findings.md` measured at a
    /// 2.3 second median freeze. libwebrtc boosts the key frame factor by at
    /// least two for the same reason; this is the lower half of that, because
    /// our own measurement put a flat 20% at 99.9% of frames delivered and the
    /// boost is insurance rather than the mechanism.
    #[must_use]
    pub const fn percent(self, kind: FrameKind) -> u8 {
        match kind {
            FrameKind::Delta => self.level,
            // The level is capped at the ceiling, which is 50, so half again is
            // at most 75 and cannot leave a byte.
            FrameKind::Key => {
                let boosted = self.level + self.level / 2;
                if boosted > Self::CEILING {
                    Self::CEILING
                } else {
                    boosted
                }
            }
        }
    }

    /// Parity shards for a frame of `data_shards` picture datagrams.
    ///
    /// Rounded up, never to nearest, so the answer is never below the
    /// percentage asked for. The two errors are not the same size: a shard too
    /// many costs one datagram, and a shard too few costs a broken reference
    /// chain and the key frame that repairs it, which is eight frames of
    /// bandwidth. Rounding up is also what keeps the smallest frames protected
    /// at all, since a one-datagram frame rounds to nothing under any other
    /// rule and at sixty a second those alone would break the chain more than
    /// once a second.
    #[must_use]
    pub fn shards(self, kind: FrameKind, data_shards: usize) -> usize {
        let percent = usize::from(self.percent(kind));
        (data_shards * percent).div_ceil(100)
    }

    /// Takes one receiver report's cumulative counters.
    ///
    /// Both are counts since the session began, so a caller passes the
    /// difference between two reports rather than the reports themselves.
    pub fn observe(&mut self, lost: u64, received: u64) {
        let Some(total) = lost.checked_add(received).filter(|total| *total > 0) else {
            return;
        };
        let wanted = Self::wanted(lost * 1000 / total);
        if wanted > self.level {
            self.level = wanted;
            self.falling = 0;
        } else if wanted + Self::SLACK <= self.level {
            self.falling += 1;
            if self.falling >= Self::HOLD {
                self.level = wanted;
                self.falling = 0;
            }
        } else {
            self.falling = 0;
        }
    }

    /// The percentage an observed loss asks for, before hysteresis.
    ///
    /// Linear in the loss, because what a frame has to survive is a number of
    /// erasures proportional to how many datagrams it puts on the path. The
    /// constant is set by the measurement: 27.9 parts per thousand asks for
    /// 20%, which is the level `spec/findings.md` measured at 99.9% of frames
    /// delivered.
    fn wanted(permille: u64) -> u8 {
        u8::try_from((permille * 18).div_ceil(25))
            .unwrap_or(Self::CEILING)
            .clamp(Self::FLOOR, Self::CEILING)
    }
}

impl Default for Parity {
    fn default() -> Self {
        Self::fixed(Self::MEASURED)
    }
}

/// The Reed-Solomon encoder, kept between frames so its working space is
/// allocated once.
#[derive(Default)]
struct Coder(Option<ReedSolomonEncoder>);

impl Coder {
    fn ready(
        &mut self,
        data: usize,
        parity: usize,
        shard_bytes: usize,
    ) -> Option<&mut ReedSolomonEncoder> {
        if self.0.is_none() {
            self.0 = Some(ReedSolomonEncoder::new(data, parity, shard_bytes).ok()?);
            return self.0.as_mut();
        }
        let coder = self.0.as_mut()?;
        coder.reset(data, parity, shard_bytes).ok()?;
        Some(coder)
    }
}

impl fmt::Debug for Coder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Coder").field(&self.0.is_some()).finish()
    }
}

/// Divides access units into datagrams and stamps the session-wide counters.
///
/// One per connection, on the host. The sequence number it writes is what lets
/// a receiver see a gap across a frame boundary, so it must not be restarted
/// for each frame.
#[derive(Debug)]
pub struct Packetiser {
    payload_len: NonZeroUsize,
    seq: Seq,
    parity: Parity,
    coder: Coder,
    /// The frame laid out as equal shards, reused between frames.
    shards: Vec<u8>,
}

impl Packetiser {
    /// Starts a session that puts `payload_len` bytes of video in each datagram.
    #[must_use]
    pub fn new(payload_len: NonZeroUsize) -> Self {
        Self {
            payload_len: even(payload_len),
            seq: Seq(0),
            parity: Parity::default(),
            coder: Coder::default(),
            shards: Vec::new(),
        }
    }

    /// Bytes of video each datagram carries.
    #[must_use]
    pub const fn payload_len(&self) -> NonZeroUsize {
        self.payload_len
    }

    /// Resizes datagrams after the path maximum transmission unit changed.
    ///
    /// The length is rounded down to an even number, because a parity shard is
    /// the same size as a picture datagram's payload and the coder works two
    /// bytes at a time. On the 1418-byte payload this transport reaches over a
    /// 1500-byte path that costs nothing at all.
    ///
    /// The sequence number carries on, because it counts datagrams rather than
    /// bytes and the receiver's gap detection depends on it being unbroken.
    pub const fn set_payload_len(&mut self, payload_len: NonZeroUsize) {
        self.payload_len = even(payload_len);
    }

    /// How much parity frames are being sent with.
    #[must_use]
    pub const fn parity(&self) -> Parity {
        self.parity
    }

    /// Sets how much parity frames are sent with.
    ///
    /// The hook the congestion controller drives once a receiver report exists
    /// to drive it from. See [`Parity`].
    pub const fn set_parity(&mut self, parity: Parity) {
        self.parity = parity;
    }

    /// Datagrams `unit` will occupy, parity included.
    ///
    /// This is the figure an admission gate must ask about. Parity is real
    /// bytes on the same queue, and a gate that counted only the picture would
    /// under-estimate by a fifth and reach the transport's eviction path, which
    /// `tachyx_transport::video` documents as fatal to the connection rather
    /// than merely lossy.
    #[must_use]
    pub fn datagrams(&self, unit: &AccessUnit<'_>) -> usize {
        let data = unit.datagrams(self.payload_len);
        data + self.parity_shards(unit, data)
    }

    /// Appends every datagram of `unit` to `out`, in send order: the picture
    /// first, then its parity.
    ///
    /// Each picture datagram is a header followed by its share of one slice. A
    /// slice never shares a datagram with another, so a lost fragment costs one
    /// slice rather than the picture.
    ///
    /// # Panics
    /// Never in practice. [`AccessUnit`] refuses more than [`MAX_SLICES`]
    /// slices, which is what keeps every slice identifier below the one parity
    /// has taken, and the parity index is drawn from the count it belongs to.
    pub fn packetise(&mut self, unit: &AccessUnit<'_>, out: &mut Vec<Vec<u8>>) {
        let last = unit.slices.len() - 1;
        // The counter runs beside the slices rather than being cast from an
        // index, because `MAX_SLICES` guarantees it fits and a cast would not
        // say so.
        for (slice, id) in unit.slices.iter().zip(0u8..) {
            let slice = *slice;
            let id = SliceId::new(id).expect("MAX_SLICES stops below the parity identifier");
            for (fragment, range) in Fragment::split(slice.len(), self.payload_len) {
                let header = PacketHeader {
                    frame: unit.id,
                    seq: self.seq,
                    kind: unit.kind,
                    role: Role::Picture(PictureHeader {
                        capture: unit.capture,
                        fragment,
                        slice: id,
                        last_slice: usize::from(id.get()) == last,
                    }),
                };
                self.seq = self.seq.next();
                let mut datagram = Vec::with_capacity(HEADER_LEN + range.len());
                datagram.extend_from_slice(&header.to_bytes());
                datagram.extend_from_slice(&slice[range]);
                out.push(datagram);
            }
        }
        self.append_parity(unit, out);
    }

    /// Builds one padding datagram of `payload` bytes and stamps it with the
    /// next sequence number.
    ///
    /// It carries no picture and no repair. What it is for is that a still
    /// desktop produces no frames at all, which is precisely when a congestion
    /// controller most needs to know whether the link has recovered, and a
    /// sequence number the receiver reports the arrival of is the whole of what
    /// measures that.
    ///
    /// It is counted in the sequence like any other datagram, because it costs
    /// the path the same bytes and its loss is the same evidence about it.
    pub fn padding(&mut self, payload: usize) -> Vec<u8> {
        let header = PacketHeader {
            frame: FrameId(0),
            seq: self.seq,
            kind: FrameKind::Delta,
            role: Role::Padding,
        };
        self.seq = self.seq.next();
        let mut datagram = Vec::with_capacity(HEADER_LEN + payload);
        datagram.extend_from_slice(&header.to_bytes());
        datagram.resize(HEADER_LEN + payload.max(1), 0);
        datagram
    }

    /// Parity shards `unit` will be sent with, given that its picture occupies
    /// `data` datagrams.
    fn parity_shards(&self, unit: &AccessUnit<'_>, data: usize) -> usize {
        if !self.codeable(unit) {
            return 0;
        }
        let shards = self.parity.shards(unit.kind, data);
        if u16::try_from(data).is_err() || !ReedSolomonEncoder::supports(data, shards) {
            // A frame with more shards than the code has room for. At the 1418
            // byte payload this transport reaches, the smallest picture that
            // reaches it is 85 megabytes, so what this really guards is a tiny
            // configured payload. Such a frame goes out unprotected rather than
            // unsent: the recovery request is still behind it.
            return 0;
        }
        shards
    }

    /// Whether parity can index this unit's datagrams at all.
    ///
    /// Parity covers the frame's datagrams in send order, and only a
    /// single-slice frame makes that index recoverable from a fragment number
    /// alone: with two slices the receiver would have to know each slice's own
    /// fragment count to place a rebuilt shard, and there is no room in sixteen
    /// bytes to say so. The encoder is configured for one slice and
    /// `spec/findings.md` recommends firmly against more, so this costs nothing
    /// today and refuses rather than misplaces a shard if that ever changes.
    fn codeable(&self, unit: &AccessUnit<'_>) -> bool {
        unit.slices.len() == 1 && self.payload_len.get().is_multiple_of(2)
    }

    fn append_parity(&mut self, unit: &AccessUnit<'_>, out: &mut Vec<Vec<u8>>) {
        let [picture] = unit.slices else { return };
        let shard_bytes = self.payload_len.get();
        let data = picture.len().div_ceil(shard_bytes);
        let (Some(data_shards), Some(count), Some(tail)) = (
            counted(data),
            counted(self.parity_shards(unit, data)),
            counted(picture.len() - (data - 1) * shard_bytes),
        ) else {
            return;
        };

        // Shard `i` is the frame's bytes `i * shard_bytes` onward, so laying
        // the whole frame into a zeroed buffer of `data` shards is one copy and
        // pads the tail at the same time.
        self.shards.clear();
        self.shards.resize(data * shard_bytes, 0);
        self.shards[..picture.len()].copy_from_slice(picture);

        let Self {
            seq, coder, shards, ..
        } = self;
        let Some(coder) = coder.ready(data, usize::from(count.get()), shard_bytes) else {
            return;
        };
        for shard in shards.chunks_exact(shard_bytes) {
            if coder.add_original_shard(shard).is_err() {
                return;
            }
        }
        let Ok(parity) = coder.encode() else { return };
        for (index, shard) in parity.recovery_iter().enumerate() {
            let index = u16::try_from(index).expect("fewer shards than the count that fit u16");
            let header = PacketHeader {
                frame: unit.id,
                seq: *seq,
                kind: unit.kind,
                role: Role::Parity(ParityHeader {
                    data_shards,
                    tail,
                    shard: Fragment::checked(index, count).expect("an index of its own count"),
                }),
            };
            *seq = seq.next();
            let mut datagram = Vec::with_capacity(HEADER_LEN + shard.len());
            datagram.extend_from_slice(&header.to_bytes());
            datagram.extend_from_slice(shard);
            out.push(datagram);
        }
    }
}

/// A count as a parity header carries it, or `None` for zero and for anything
/// too large to name in two bytes.
fn counted(count: usize) -> Option<NonZeroU16> {
    NonZeroU16::new(u16::try_from(count).ok()?)
}

/// Rounds a payload length down to an even number of bytes.
///
/// `NonZeroUsize::new` fails only for a one-byte payload, which is a datagram
/// of seventeen bytes and no transport this runs over. That length is kept as
/// it is and [`Packetiser::codeable`] refuses parity for it, rather than
/// producing a shard the coder cannot take.
const fn even(payload_len: NonZeroUsize) -> NonZeroUsize {
    match NonZeroUsize::new(payload_len.get() & !1) {
        Some(even) => even,
        None => payload_len,
    }
}

/// An access unit rebuilt from its datagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Which frame this is.
    pub id: FrameId,
    /// When the host captured it.
    pub capture: Timestamp,
    /// Whether it stands alone.
    pub kind: FrameKind,
    /// The encoded picture, slices concatenated in decode order.
    pub data: Vec<u8>,
}

/// How long a frame waits once a signal says a fragment of it is missing.
///
/// Two signals prove a frame will not be finished by anything still in flight:
/// a gap in the sequence number between two of its own packets, and a packet of
/// a later frame, since the host sends a frame's packets before the next
/// frame's. Neither is acted on at once, because both also fire on a packet
/// that was merely reordered.
///
/// Measured on one air hop: 22.92% of packets arrive more than 2 ms behind the
/// fastest one, and against a window of 2 ms a jitter-only link with no loss at
/// all delivered 1.8% of frames. At 15 ms the same link delivered 94.6%. The
/// window costs nothing to widen, because it only ever applies to a frame
/// already known to be damaged: an undamaged frame is delivered the instant its
/// last datagram arrives and never looks at it. Its ceiling is a frame period,
/// past which a later frame has already moved the receiver's floor.
pub const REORDER_WINDOW: Duration = Duration::from_millis(15);

/// What a reassembler has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    /// Datagrams parsed, parity included.
    pub packets_received: u64,
    /// Datagrams the sequence number says never arrived.
    pub packets_lost: u64,
    /// Picture datagrams for a frame that had already been decided.
    ///
    /// Parity is deliberately not counted here. A frame is delivered on its
    /// last picture datagram and its parity is sent behind that, so the parity
    /// of every healthy frame arrives after the frame is decided, and counting
    /// it late would report a fifth of the stream as late on a perfect link.
    pub packets_late: u64,
    /// Parity datagrams parsed.
    pub parity_received: u64,
    /// Frames handed to the decoder.
    pub frames_delivered: u64,
    /// Frames of those that were rebuilt from parity rather than arriving
    /// whole.
    pub frames_recovered: u64,
    /// Frames abandoned with fragments missing that parity could not replace.
    pub frames_dropped: u64,
    /// Frames the sender numbered and of which nothing arrived at all.
    ///
    /// Counted separately from [`ReassemblyStats::frames_dropped`] because
    /// nothing in the frame itself reveals it: the hole in the identifiers is
    /// the only evidence that the frame existed.
    pub frames_missing: u64,
    /// Frames that arrived whole and were thrown away anyway, because the
    /// frames they refer back to did not.
    pub frames_undecodable: u64,
}

/// The Reed-Solomon decoder, kept between frames so its working space is
/// allocated once.
#[derive(Default)]
struct Repairer(Option<ReedSolomonDecoder>);

impl Repairer {
    fn ready(
        &mut self,
        data: usize,
        parity: usize,
        shard_bytes: usize,
    ) -> Option<&mut ReedSolomonDecoder> {
        if self.0.is_none() {
            self.0 = Some(ReedSolomonDecoder::new(data, parity, shard_bytes).ok()?);
            return self.0.as_mut();
        }
        let coder = self.0.as_mut()?;
        coder.reset(data, parity, shard_bytes).ok()?;
        Some(coder)
    }
}

impl fmt::Debug for Repairer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Repairer").field(&self.0.is_some()).finish()
    }
}

/// Rebuilds access units from datagrams, giving up on a frame rather than
/// stalling behind it.
///
/// A frame every packet of which was lost leaves no evidence but the hole it
/// makes in the identifiers, so the identifiers are what this reads. That is
/// only sound because the sender numbers every picture it encodes and skips
/// none: see [`FrameId`].
///
/// Frame identifiers are compared directly rather than circularly. The four
/// bytes they occupy last four hundred days at a hundred and twenty frames a
/// second, which outlives any session.
///
/// Completed pictures are queued rather than returned, because one datagram can
/// finish more than one of them: the datagram that completes a frame is also
/// what decides the damaged frames below it, and any of those the parity can
/// rebuild are delivered ahead of it. Callers take them with
/// [`Reassembler::pop`].
#[derive(Debug)]
pub struct Reassembler {
    deadline: Duration,
    open: BTreeMap<u32, Partial>,
    ready: VecDeque<Frame>,
    previous: Option<Marker>,
    /// Frames below this have been delivered, dropped, or passed over. Their
    /// packets are late rather than useful.
    floor: u32,
    needs_key: bool,
    coder: Repairer,
    stats: ReassemblyStats,
}

/// Where the last accepted datagram sat in the session.
#[derive(Debug, Clone, Copy)]
struct Marker {
    seq: Seq,
    frame: FrameId,
}

#[derive(Debug)]
struct Partial {
    capture: Timestamp,
    kind: FrameKind,
    deadline: Instant,
    last_slice: Option<u8>,
    slices: BTreeMap<u8, PartialSlice>,
    parity: Option<ParityBlock>,
}

/// Fragments of one slice, keyed by index so that nothing is allocated for a
/// fragment that has not arrived and a hostile count cannot be amplified into
/// memory.
#[derive(Debug)]
struct PartialSlice {
    count: NonZeroU16,
    fragments: BTreeMap<u16, Vec<u8>>,
}

/// The repair for one frame, as much of it as has arrived.
///
/// Every field but the shards themselves comes from the first parity packet of
/// the frame. A later one that disagrees is a sender fault and is discarded:
/// the shards of one coding block have to belong to the same block.
#[derive(Debug)]
struct ParityBlock {
    data_shards: NonZeroU16,
    parity_shards: NonZeroU16,
    tail: NonZeroU16,
    shard_bytes: usize,
    shards: BTreeMap<u16, Vec<u8>>,
}

impl PartialSlice {
    fn complete(&self) -> bool {
        self.fragments.len() == usize::from(self.count.get())
    }
}

impl Partial {
    fn complete(&self) -> bool {
        let Some(last) = self.last_slice else {
            return false;
        };
        // Slice identifiers run from zero, so a map holding one more entry than
        // the last identifier holds every slice exactly once.
        self.slices.len() == usize::from(last) + 1
            && self.slices.values().all(PartialSlice::complete)
    }

    fn into_frame(self, id: FrameId) -> Frame {
        let mut data = Vec::with_capacity(
            self.slices
                .values()
                .flat_map(|slice| slice.fragments.values())
                .map(Vec::len)
                .sum(),
        );
        for slice in self.slices.into_values() {
            for fragment in slice.fragments.into_values() {
                data.extend_from_slice(&fragment);
            }
        }
        Frame {
            id,
            capture: self.capture,
            kind: self.kind,
            data,
        }
    }
}

impl Reassembler {
    /// Starts a receiver that waits at most `deadline` for a frame to arrive
    /// whole.
    ///
    /// This is the fallback signal, for the frame nothing follows: the host's
    /// capture blocks while the screen is static, with idle gaps measured up to
    /// 1.53 seconds, so the last frame before an idle period has no successor
    /// to prove it is finished.
    #[must_use]
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            open: BTreeMap::new(),
            ready: VecDeque::new(),
            previous: None,
            floor: 0,
            // Nothing before the first key frame can be decoded.
            needs_key: true,
            coder: Repairer::default(),
            stats: ReassemblyStats::default(),
        }
    }

    /// Whether the decoder cannot continue until a key frame arrives.
    ///
    /// Set by every dropped frame and cleared by the next complete key frame. A
    /// frame parity rebuilt is not a dropped frame and does not set it, which
    /// is the whole point of carrying parity.
    ///
    /// While it holds, complete delta frames are thrown away rather than
    /// decoded: neither the encoder's intra-refresh nor its long-term reference
    /// frames are reachable on this host, so a delta frame whose references
    /// were lost corrupts the picture until the next key frame regardless.
    #[must_use]
    pub const fn needs_keyframe(&self) -> bool {
        self.needs_key
    }

    /// What this receiver has seen.
    #[must_use]
    pub const fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// The next picture ready for the decoder, in frame order.
    ///
    /// Drained after every [`Reassembler::push`] and every
    /// [`Reassembler::expire`], both of which can leave more than one.
    pub fn pop(&mut self) -> Option<Frame> {
        self.ready.pop_front()
    }

    /// When the earliest open frame gives up, if any frame is open.
    ///
    /// A receiver waits on this rather than on a fixed tick, so a session
    /// watching a static screen costs no wakeups.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.open.values().map(|partial| partial.deadline).min()
    }

    /// Takes one datagram.
    ///
    /// Runs [`Reassembler::expire`] first, so a caller that keeps feeding
    /// datagrams needs no timer of its own. Whatever this completed or rebuilt
    /// comes out of [`Reassembler::pop`].
    ///
    /// # Errors
    /// If the datagram is not a video packet.
    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Result<(), MalformedPacket> {
        self.expire(now);
        let (header, payload) = PacketHeader::parse(datagram)?;
        self.stats.packets_received += 1;

        let gap_within_frame = self.track_sequence(&header);
        // Padding takes part in the sequence accounting above and in nothing
        // else. Every field that would say which frame it belongs to means
        // nothing on it, so it must not reach reassembly.
        if matches!(header.role, Role::Padding) {
            return Ok(());
        }
        let parity = matches!(header.role, Role::Parity(_));
        if parity {
            self.stats.parity_received += 1;
        }
        if header.frame.0 < self.floor {
            if !parity {
                self.stats.packets_late += 1;
            }
            return Ok(());
        }
        if header.frame.0 > self.floor {
            for partial in self.open.range_mut(..header.frame.0).map(|(_, v)| v) {
                partial.deadline = partial.deadline.min(now + REORDER_WINDOW);
            }
        }
        match header.role {
            Role::Parity(shard) => self.take_parity(header.frame, shard, payload, now),
            Role::Picture(picture) => {
                self.take_picture(header.frame, header.kind, picture, payload, now);
            }
            // Decided above, where the sequence accounting it does take part in
            // happens.
            Role::Padding => {}
        }
        if gap_within_frame {
            // The missing packets sit between two packets of this frame, so
            // they were this frame's. The packet in hand is still kept: it may
            // be the reordered one that fills the hole.
            if let Some(partial) = self.open.get_mut(&header.frame.0) {
                partial.deadline = partial.deadline.min(now + REORDER_WINDOW);
            }
        }
        Ok(())
    }

    /// Files one parity shard against a frame that is already open.
    ///
    /// A repair never opens a frame of its own. Recovery needs as many shards
    /// as the frame had picture datagrams, so a frame of which no picture
    /// datagram arrived is rebuildable only where the parity outnumbers the
    /// picture, which is above [`Parity::CEILING`] and never happens. Keeping
    /// the rule means the capture time and the key-frame flag are always the
    /// picture's to state, and neither has to travel on a repair.
    fn take_parity(&mut self, frame: FrameId, shard: ParityHeader, payload: &[u8], now: Instant) {
        let Some(partial) = self.open.get_mut(&frame.0) else {
            return;
        };
        // Parity arrives behind the picture it repairs, so a frame still open
        // when it lands is a frame with a hole in it.
        partial.deadline = partial.deadline.min(now + REORDER_WINDOW);
        let block = partial.parity.get_or_insert_with(|| ParityBlock {
            data_shards: shard.data_shards,
            parity_shards: shard.shard.count(),
            tail: shard.tail,
            shard_bytes: payload.len(),
            shards: BTreeMap::new(),
        });
        if block.data_shards != shard.data_shards
            || block.parity_shards != shard.shard.count()
            || block.tail != shard.tail
            || block.shard_bytes != payload.len()
        {
            return;
        }
        block
            .shards
            .entry(shard.shard.index())
            .or_insert_with(|| payload.to_vec());
    }

    /// Files one picture fragment, delivering the frame if it completes it.
    fn take_picture(
        &mut self,
        frame: FrameId,
        kind: FrameKind,
        picture: PictureHeader,
        payload: &[u8],
        now: Instant,
    ) {
        let partial = match self.open.entry(frame.0) {
            Entry::Occupied(open) => open.into_mut(),
            Entry::Vacant(slot) => slot.insert(Partial {
                capture: picture.capture,
                kind,
                deadline: now + self.deadline,
                last_slice: None,
                slices: BTreeMap::new(),
                parity: None,
            }),
        };
        if picture.last_slice {
            partial.last_slice = Some(picture.slice.get());
        }
        // A fragment count that disagrees with the one an earlier packet of the
        // same slice carried is a sender fault. The first count wins and the
        // frame times out if that leaves it unfinishable.
        partial
            .slices
            .entry(picture.slice.get())
            .or_insert_with(|| PartialSlice {
                count: picture.fragment.count(),
                fragments: BTreeMap::new(),
            })
            .fragments
            .entry(picture.fragment.index())
            .or_insert_with(|| payload.to_vec());
        if !partial.complete() {
            return;
        }
        let partial = self.open.remove(&frame.0).expect("filled a moment ago");
        self.settle_below(frame.0);
        self.floor = frame.0 + 1;
        self.deliver(partial.into_frame(frame));
    }

    /// Drops frames whose wait has run out, rebuilding the ones parity can
    /// still save.
    ///
    /// Only needed on a connection where no datagram is arriving to run it, so
    /// on a timer beside the receive loop rather than in it. Whatever it
    /// rebuilt comes out of [`Reassembler::pop`].
    pub fn expire(&mut self, now: Instant) {
        let Some(highest) = self
            .open
            .iter()
            .filter(|(_, partial)| partial.deadline <= now)
            .map(|(&id, _)| id)
            .max()
        else {
            return;
        };
        // Everything at or below the frame that gave up is decided with it: a
        // frame below the floor can no longer be completed, because its
        // remaining packets arrive as late ones.
        self.settle_below(highest + 1);
    }

    /// Accounts for the sequence number, reporting whether the gap it reveals
    /// provably belongs to the arriving packet's own frame.
    fn track_sequence(&mut self, header: &PacketHeader) -> bool {
        let marker = Marker {
            seq: header.seq,
            frame: header.frame,
        };
        let Some(previous) = self.previous else {
            self.previous = Some(marker);
            return false;
        };
        let ahead = header.seq.distance_from(previous.seq.next());
        if ahead < 0 {
            // Reordered or duplicated. It stays out of the record, so that the
            // packets it appeared to skip are not counted lost a second time.
            return false;
        }
        self.previous = Some(marker);
        if ahead == 0 {
            return false;
        }
        self.stats.packets_lost += u64::from(ahead.unsigned_abs());
        // A gap that straddles a frame boundary could be the tail of the older
        // frame or the head of the newer one. The older frame is finished by
        // the arrival of the newer one anyway, and the newer one is left to its
        // own accounting rather than guessed at.
        previous.frame == header.frame
    }

    /// Decides every frame below `id`, which nothing can complete any more.
    ///
    /// Frames still open are the damaged ones, and each gets one attempt at
    /// being rebuilt from its parity before it is given up. The rest of the
    /// span is frames of which not one packet arrived, which nothing but the
    /// gap in the identifiers reveals.
    ///
    /// Walked in identifier order so that a rebuilt frame is queued behind the
    /// frames it refers to and ahead of the ones that refer to it, and so that
    /// a hole before it is what marks it undecodable.
    fn settle_below(&mut self, id: u32) {
        if id <= self.floor {
            return;
        }
        let damaged: Vec<u32> = self.open.range(..id).map(|(&open, _)| open).collect();
        let mut cursor = self.floor;
        for open in damaged {
            if open > cursor {
                self.stats.frames_missing += u64::from(open - cursor);
                self.needs_key = true;
            }
            let partial = self.open.remove(&open).expect("listed a moment ago");
            if let Some(frame) = self.recover(FrameId(open), &partial) {
                self.stats.frames_recovered += 1;
                self.deliver(frame);
            } else {
                self.stats.frames_dropped += 1;
                self.needs_key = true;
            }
            cursor = open + 1;
        }
        if id > cursor {
            self.stats.frames_missing += u64::from(id - cursor);
            self.needs_key = true;
        }
        self.floor = id;
    }

    /// Queues one picture, or throws it away because its references are gone.
    fn deliver(&mut self, frame: Frame) {
        if matches!(frame.kind, FrameKind::Key) {
            self.needs_key = false;
        } else if self.needs_key {
            self.stats.frames_undecodable += 1;
            return;
        }
        self.stats.frames_delivered += 1;
        self.ready.push_back(frame);
    }

    /// Rebuilds a damaged frame from its parity, or says it cannot.
    ///
    /// Reached only from [`Reassembler::settle_below`], so only for a frame
    /// already known to be damaged and already past its reorder window or
    /// overtaken by a later frame. A stream losing nothing never reaches it: an
    /// undamaged frame is delivered by the datagram that completes it and never
    /// becomes a candidate.
    fn recover(&mut self, id: FrameId, partial: &Partial) -> Option<Frame> {
        let block = partial.parity.as_ref()?;
        let data = usize::from(block.data_shards.get());
        let shard_bytes = block.shard_bytes;
        let tail = usize::from(block.tail.get());
        // Parity indexes the frame's datagrams in send order, which is the
        // fragment index only while the frame is one slice. The sender refuses
        // to code anything else; a frame that says otherwise is a fault.
        let mut slices = partial.slices.iter();
        let (Some((0, received)), None) = (slices.next(), slices.next()) else {
            return None;
        };
        if usize::from(received.count.get()) != data
            || received.fragments.len() + block.shards.len() < data
            || tail > shard_bytes
        {
            return None;
        }

        let coder = self
            .coder
            .ready(data, usize::from(block.parity_shards.get()), shard_bytes)?;
        let mut padded = Vec::new();
        for (&index, bytes) in &received.fragments {
            let last = usize::from(index) + 1 == data;
            let expected = if last { tail } else { shard_bytes };
            if bytes.len() != expected {
                return None;
            }
            let shard: &[u8] = if last {
                padded.clear();
                padded.extend_from_slice(bytes);
                padded.resize(shard_bytes, 0);
                &padded
            } else {
                bytes
            };
            coder.add_original_shard(usize::from(index), shard).ok()?;
        }
        for (&index, shard) in &block.shards {
            coder
                .add_recovery_shard(usize::from(index), shard)
                .ok()
                .filter(|()| shard.len() == shard_bytes)?;
        }
        let repaired = coder.decode().ok()?;
        let rebuilt: BTreeMap<usize, &[u8]> = repaired.restored_original_iter().collect();

        let mut out = Vec::with_capacity((data - 1) * shard_bytes + tail);
        for index in 0..data {
            let take = if index + 1 == data { tail } else { shard_bytes };
            match received.fragments.get(&u16::try_from(index).ok()?) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => out.extend_from_slice(rebuilt.get(&index)?.get(..take)?),
            }
        }
        Some(Frame {
            id,
            capture: partial.capture,
            kind: partial.kind,
            data: out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: NonZeroUsize = NonZeroUsize::new(64).unwrap();

    fn unit<'a>(id: u32, kind: FrameKind, slices: &'a [&'a [u8]]) -> AccessUnit<'a> {
        AccessUnit::new(
            FrameId(id),
            Timestamp::from_session_micros(u64::from(id) * 16_600),
            kind,
            slices,
        )
        .expect("well formed")
    }

    fn picture(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from(i % 251).expect("under 251") ^ seed)
            .collect()
    }

    /// A packetiser that carries no parity, for the tests about everything
    /// else.
    fn bare(payload: NonZeroUsize) -> Packetiser {
        let mut packetiser = Packetiser::new(payload);
        packetiser.set_parity(Parity::fixed(0));
        packetiser
    }

    fn deliver(reassembler: &mut Reassembler, datagrams: &[Vec<u8>], now: Instant) -> Vec<Frame> {
        let mut frames = Vec::new();
        for datagram in datagrams {
            reassembler.push(datagram, now).expect("well formed");
            while let Some(frame) = reassembler.pop() {
                frames.push(frame);
            }
        }
        frames
    }

    fn drain(reassembler: &mut Reassembler) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = reassembler.pop() {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn a_single_slice_frame_survives_the_round_trip() {
        let picture = picture(1000, 0x5A);
        let mut packetiser = bare(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);

        assert_eq!(out.len(), 1000_usize.div_ceil(64));
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let frames = deliver(&mut reassembler, &out, Instant::now());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, picture);
        assert_eq!(frames[0].id, FrameId(0));
        assert_eq!(frames[0].kind, FrameKind::Key);
        assert!(!reassembler.needs_keyframe());
    }

    #[test]
    fn padding_is_counted_in_the_sequence_and_reaches_nothing_else() {
        let picture = picture(200, 0x11);
        let mut packetiser = bare(PAYLOAD);
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();

        let mut first = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut first);
        assert_eq!(deliver(&mut reassembler, &first, now).len(), 1);

        // A probe between two pictures. It must leave no hole in the sequence
        // and must not be read as a slice of either frame.
        let padding: Vec<Vec<u8>> = (0..5).map(|_| packetiser.padding(64)).collect();
        assert!(deliver(&mut reassembler, &padding, now).is_empty());

        let mut second = Vec::new();
        packetiser.packetise(&unit(1, FrameKind::Delta, &[&picture]), &mut second);
        assert_eq!(deliver(&mut reassembler, &second, now).len(), 1);

        let stats = reassembler.stats();
        assert_eq!(stats.packets_lost, 0, "padding fills its own sequence");
        assert_eq!(stats.packets_late, 0);
        assert_eq!(stats.frames_dropped, 0);
        assert_eq!(stats.frames_missing, 0);
        assert_eq!(
            stats.packets_received,
            (first.len() + padding.len() + second.len()) as u64,
            "padding is a datagram the path carried, so the loss rate counts it"
        );
    }

    #[test]
    fn slices_are_concatenated_in_decode_order() {
        let parameter_sets_with_first_slice = picture(300, 1);
        let second = picture(200, 2);
        let slices: [&[u8]; 2] = [&parameter_sets_with_first_slice, &second];
        let mut packetiser = bare(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &slices), &mut out);

        // Fragmentation is per slice, so the short tail of slice zero does not
        // share a datagram with the head of slice one.
        assert_eq!(out.len(), 300_usize.div_ceil(64) + 200_usize.div_ceil(64));

        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let frames = deliver(&mut reassembler, &out, Instant::now());
        let mut expected = parameter_sets_with_first_slice.clone();
        expected.extend_from_slice(&second);
        assert_eq!(frames[0].data, expected);
    }

    #[test]
    fn reordering_within_a_frame_costs_nothing() {
        let picture = picture(1000, 3);
        let mut packetiser = bare(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);
        out.swap(2, 9);
        out.reverse();

        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let frames = deliver(&mut reassembler, &out, Instant::now());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, picture);
        assert_eq!(reassembler.stats().frames_dropped, 0);
    }

    #[test]
    fn a_lost_fragment_drops_its_frame_and_asks_for_a_key_frame() {
        let key = picture(600, 4);
        let delta = picture(600, 5);
        let mut packetiser = bare(PAYLOAD);
        let mut first = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&key]), &mut first);
        let mut second = Vec::new();
        packetiser.packetise(&unit(1, FrameKind::Delta, &[&delta]), &mut second);

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        assert_eq!(deliver(&mut reassembler, &first, start).len(), 1);

        second.remove(3);
        assert!(deliver(&mut reassembler, &second, start).is_empty());
        // The gap is between two packets of frame one, so the frame is armed
        // rather than left to the full deadline.
        reassembler.expire(start + REORDER_WINDOW + Duration::from_millis(1));
        assert_eq!(reassembler.stats().frames_dropped, 1);
        assert_eq!(reassembler.stats().packets_lost, 1);
        assert!(reassembler.needs_keyframe());
    }

    #[test]
    fn a_newer_frame_finishes_an_incomplete_older_one() {
        let mut packetiser = bare(PAYLOAD);
        let one = picture(600, 6);
        let two = picture(600, 7);
        let mut first = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&one]), &mut first);
        let mut second = Vec::new();
        packetiser.packetise(&unit(1, FrameKind::Key, &[&two]), &mut second);

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_secs(10));
        // Lose the tail of frame zero, so nothing inside it reveals the loss.
        for datagram in &first[..first.len() - 1] {
            reassembler.push(datagram, start).unwrap();
            assert!(reassembler.pop().is_none());
        }
        let frames = deliver(&mut reassembler, &second, start);
        assert_eq!(frames.len(), 1, "frame one is not held up by frame zero");
        assert_eq!(frames[0].id, FrameId(1));
        assert_eq!(
            reassembler.stats().frames_dropped,
            1,
            "frame zero is reported dropped rather than left open for ten seconds"
        );
    }

    #[test]
    fn the_last_frame_before_an_idle_screen_times_out() {
        let mut packetiser = bare(PAYLOAD);
        let picture = picture(600, 8);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        for datagram in &out[..out.len() - 1] {
            reassembler.push(datagram, start).unwrap();
            assert!(reassembler.pop().is_none());
        }
        reassembler.expire(start + Duration::from_millis(49));
        assert_eq!(reassembler.stats().frames_dropped, 0);
        reassembler.expire(start + Duration::from_millis(51));
        assert_eq!(reassembler.stats().frames_dropped, 1);
        assert!(reassembler.needs_keyframe());
    }

    #[test]
    fn a_delta_frame_is_thrown_away_until_a_key_frame_resynchronises() {
        let mut packetiser = bare(PAYLOAD);
        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));

        let delta = picture(200, 9);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Delta, &[&delta]), &mut out);
        assert!(deliver(&mut reassembler, &out, start).is_empty());
        assert_eq!(reassembler.stats().frames_undecodable, 1);
        assert!(reassembler.needs_keyframe());

        let key = picture(200, 10);
        let mut out = Vec::new();
        packetiser.packetise(&unit(1, FrameKind::Key, &[&key]), &mut out);
        assert_eq!(deliver(&mut reassembler, &out, start).len(), 1);
        assert!(!reassembler.needs_keyframe());
    }

    #[test]
    fn a_frame_that_arrives_not_at_all_is_still_loss() {
        let mut packetiser = bare(PAYLOAD);
        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));

        let mut wire = Vec::new();
        for (id, kind) in [
            (0, FrameKind::Key),
            (1, FrameKind::Delta),
            (2, FrameKind::Delta),
        ] {
            let picture = picture(200, u8::try_from(id).unwrap());
            let mut out = Vec::new();
            packetiser.packetise(&unit(id, kind, &[&picture]), &mut out);
            wire.push(out);
        }

        assert_eq!(deliver(&mut reassembler, &wire[0], start).len(), 1);
        assert!(!reassembler.needs_keyframe());
        // Frame one is not merely damaged: every packet of it is gone, so
        // nothing inside the frame and nothing about frame two's own packets
        // says anything is missing. Only the hole in the identifiers does.
        assert!(
            deliver(&mut reassembler, &wire[2], start).is_empty(),
            "frame two refers back to a frame the decoder never received"
        );
        assert_eq!(reassembler.stats().frames_missing, 1);
        assert_eq!(reassembler.stats().frames_undecodable, 1);
        assert!(reassembler.needs_keyframe());
    }

    #[test]
    fn a_frame_lost_whole_between_two_key_frames_costs_nothing() {
        let mut packetiser = bare(PAYLOAD);
        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));

        let mut wire = Vec::new();
        for id in 0..3u32 {
            let picture = picture(200, u8::try_from(id).unwrap());
            let mut out = Vec::new();
            packetiser.packetise(&unit(id, FrameKind::Key, &[&picture]), &mut out);
            wire.push(out);
        }

        assert_eq!(deliver(&mut reassembler, &wire[0], start).len(), 1);
        assert_eq!(
            deliver(&mut reassembler, &wire[2], start).len(),
            1,
            "a key frame stands alone, so the hole before it is counted and forgiven"
        );
        assert_eq!(reassembler.stats().frames_missing, 1);
        assert!(!reassembler.needs_keyframe());
    }

    #[test]
    fn a_late_packet_for_a_decided_frame_is_counted_and_ignored() {
        let mut packetiser = bare(PAYLOAD);
        let picture = picture(200, 11);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);
        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        assert_eq!(deliver(&mut reassembler, &out, start).len(), 1);
        reassembler.push(&out[0], start).unwrap();
        assert!(reassembler.pop().is_none());
        assert_eq!(reassembler.stats().packets_late, 1);
        assert_eq!(reassembler.stats().frames_delivered, 1);
    }

    #[test]
    fn induced_loss_and_reordering_never_yields_a_corrupt_frame() {
        let mut packetiser = bare(PAYLOAD);
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let start = Instant::now();
        let mut pictures = Vec::new();
        let mut wire: Vec<(u32, Vec<u8>)> = Vec::new();
        for id in 0..60u32 {
            let seed = u8::try_from(id).unwrap();
            let picture = picture(300 + usize::try_from(id).unwrap() * 11, seed);
            let kind = if id % 5 == 0 {
                FrameKind::Key
            } else {
                FrameKind::Delta
            };
            let mut out = Vec::new();
            packetiser.packetise(&unit(id, kind, &[&picture]), &mut out);
            wire.extend(out.into_iter().map(|datagram| (id, datagram)));
            pictures.push(picture);
        }

        // A deterministic cull and a deterministic shuffle: every fifty-third
        // packet is lost, every ninth frame is lost whole, and every seventh
        // neighbouring pair is swapped.
        let mut damaged = Vec::new();
        let mut kept: Vec<(u32, Vec<u8>)> = Vec::new();
        for (index, packet) in wire.into_iter().enumerate() {
            if index % 53 == 17 || packet.0 % 9 == 4 {
                damaged.push(packet.0);
            } else {
                kept.push(packet);
            }
        }
        for index in (0..kept.len() - 1).step_by(7) {
            kept.swap(index, index + 1);
        }

        let mut delivered = 0u64;
        let mut previous: Option<u32> = None;
        for (_, datagram) in &kept {
            reassembler.push(datagram, start).expect("well formed");
            while let Some(frame) = reassembler.pop() {
                assert_eq!(
                    &frame.data,
                    &pictures[usize::try_from(frame.id.0).unwrap()],
                    "frame {} came back wrong",
                    frame.id.0
                );
                assert!(
                    !damaged.contains(&frame.id.0),
                    "frame {} lost a packet and was delivered anyway",
                    frame.id.0
                );
                // What a delta frame refers to is the frame before it, so a
                // delta may only be delivered when that frame was delivered
                // too. This is the property a wholly lost frame breaks: it
                // takes every packet with it, so nothing but the hole in the
                // identifiers says the reference is gone.
                if matches!(frame.kind, FrameKind::Delta) {
                    assert_eq!(
                        previous,
                        frame.id.0.checked_sub(1),
                        "delta frame {} was decoded without the frame it refers to",
                        frame.id.0
                    );
                }
                previous = Some(frame.id.0);
                delivered += 1;
            }
        }
        assert!(
            delivered > 10,
            "loss this sparse must still let most frames through, got {delivered}"
        );
        assert!(reassembler.stats().frames_dropped > 0);
        assert_eq!(delivered, reassembler.stats().frames_delivered);
    }

    #[test]
    fn a_malformed_access_unit_is_refused() {
        let empty: [&[u8]; 0] = [];
        assert_eq!(
            AccessUnit::new(
                FrameId(0),
                Timestamp::from_session_micros(0),
                FrameKind::Key,
                &empty,
            ),
            Err(MalformedUnit::Empty)
        );
        let hollow: [&[u8]; 1] = [&[]];
        assert_eq!(
            AccessUnit::new(
                FrameId(0),
                Timestamp::from_session_micros(0),
                FrameKind::Key,
                &hollow,
            ),
            Err(MalformedUnit::EmptySlice)
        );
        let one: &[u8] = &[1];
        let many = vec![one; MAX_SLICES + 1];
        assert_eq!(
            AccessUnit::new(
                FrameId(0),
                Timestamp::from_session_micros(0),
                FrameKind::Key,
                &many,
            ),
            Err(MalformedUnit::TooManySlices)
        );
    }

    #[test]
    fn datagram_counts_are_predicted_before_the_frame_is_sent() {
        let a = picture(1000, 12);
        let b = picture(65, 13);
        let slices: [&[u8]; 2] = [&a, &b];
        let unit = unit(0, FrameKind::Key, &slices);
        assert_eq!(unit.encoded_len(), 1065);
        assert_eq!(unit.datagrams(PAYLOAD), 16 + 2);

        let mut packetiser = bare(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit, &mut out);
        assert_eq!(out.len(), unit.datagrams(PAYLOAD));
        assert_eq!(out.len(), packetiser.datagrams(&unit));
    }

    #[test]
    fn the_gate_is_told_about_the_parity_as_well_as_the_picture() {
        let picture = picture(12 * 64, 14);
        let slices: [&[u8]; 1] = [&picture];
        let mut packetiser = Packetiser::new(PAYLOAD);
        for kind in [FrameKind::Key, FrameKind::Delta] {
            let unit = unit(0, kind, &slices);
            let mut out = Vec::new();
            packetiser.packetise(&unit, &mut out);
            assert_eq!(
                out.len(),
                packetiser.datagrams(&unit),
                "the gate must count every datagram the packetiser produces"
            );
            assert!(
                packetiser.datagrams(&unit) > unit.datagrams(PAYLOAD),
                "parity is real bytes on the same queue"
            );
        }
    }

    /// Every parity datagram of a frame names the same coding block, and no
    /// picture datagram can be mistaken for one.
    #[test]
    fn parity_datagrams_describe_the_block_they_repair() {
        let picture = picture(10 * 64 - 7, 15);
        let mut packetiser = Packetiser::new(PAYLOAD);
        packetiser.set_parity(Parity::fixed(20));
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Delta, &[&picture]), &mut out);
        assert_eq!(out.len(), 12, "ten of picture and two of parity");

        for (index, datagram) in out.iter().enumerate() {
            let (header, payload) = PacketHeader::parse(datagram).expect("well formed");
            assert_eq!(header.frame, FrameId(0));
            match header.role {
                Role::Picture(_) => assert!(index < 10),
                Role::Parity(parity) => {
                    assert_eq!(parity.data_shards.get(), 10);
                    assert_eq!(usize::from(parity.tail.get()), 10 * 64 - 7 - 9 * 64);
                    assert_eq!(parity.shard.count().get(), 2);
                    assert_eq!(usize::from(parity.shard.index()), index - 10);
                    assert_eq!(payload.len(), 64, "a shard is one payload wide");
                }
                Role::Padding => panic!("packetising a picture produces no padding"),
            }
        }
    }

    #[test]
    fn a_frame_recovers_from_every_pattern_of_parity_losses() {
        let picture = picture(8 * 64 - 5, 16);
        let mut packetiser = Packetiser::new(PAYLOAD);
        packetiser.set_parity(Parity::fixed(20));
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);
        let parity = out.len() - 8;
        assert_eq!(parity, 3, "eight datagrams boosted to thirty percent");

        let start = Instant::now();
        let mut lost = Vec::new();
        let mut cases = 0;
        subsets(out.len(), parity, &mut lost, &mut |lost: &[usize]| {
            cases += 1;
            let mut reassembler = Reassembler::new(Duration::from_millis(50));
            for (index, datagram) in out.iter().enumerate() {
                if lost.contains(&index) {
                    continue;
                }
                reassembler.push(datagram, start).expect("well formed");
            }
            reassembler.expire(start + Duration::from_millis(51));
            let frames = drain(&mut reassembler);
            assert_eq!(
                frames.len(),
                1,
                "losing datagrams {lost:?} of {} must still be repairable",
                out.len()
            );
            assert_eq!(frames[0].data, picture, "rebuilt frame came back wrong");
            assert!(!reassembler.needs_keyframe());
        });
        // Eleven datagrams choose three.
        assert_eq!(cases, 165);
    }

    /// Calls `each` with every subset of `take` of the numbers below `of`.
    fn subsets(of: usize, take: usize, chosen: &mut Vec<usize>, each: &mut impl FnMut(&[usize])) {
        if chosen.len() == take {
            each(chosen);
            return;
        }
        let from = chosen.last().map_or(0, |last| last + 1);
        for index in from..of {
            chosen.push(index);
            subsets(of, take, chosen, each);
            chosen.pop();
        }
    }

    #[test]
    fn recovery_does_nothing_when_nothing_is_lost() {
        let picture = picture(1000, 17);
        let mut packetiser = Packetiser::new(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let frames = deliver(&mut reassembler, &out, start);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, picture);
        // The frame is delivered by the datagram that completes it, and the
        // parity behind it never reaches the decoder. Nothing on the healthy
        // path touches the coder at all.
        assert_eq!(reassembler.stats().frames_recovered, 0);
        assert_eq!(reassembler.stats().packets_late, 0);
        assert!(reassembler.stats().parity_received > 0);
    }

    #[test]
    fn a_frame_missing_more_than_its_parity_is_abandoned_cleanly() {
        let picture = picture(8 * 64, 18);
        let mut packetiser = Packetiser::new(PAYLOAD);
        packetiser.set_parity(Parity::fixed(20));
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);
        let parity = out.len() - 8;

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        for datagram in out.iter().skip(parity + 1) {
            reassembler.push(datagram, start).expect("well formed");
        }
        reassembler.expire(start + Duration::from_millis(51));
        assert!(drain(&mut reassembler).is_empty());
        assert_eq!(reassembler.stats().frames_recovered, 0);
        assert_eq!(reassembler.stats().frames_dropped, 1);
        assert!(reassembler.needs_keyframe());
    }

    #[test]
    fn a_frame_with_more_shards_than_the_code_holds_goes_out_unprotected() {
        // Two bytes a datagram is the smallest shard the coder takes, so this
        // is the cheapest way to reach a shard count the code cannot cover:
        // 60,000 of picture asks for 12,000 of parity, and the field has room
        // for 65,536 once the parity count is rounded up to a power of two.
        let tiny = NonZeroUsize::new(2).expect("nonzero");
        let picture = picture(120_000, 19);
        let mut packetiser = Packetiser::new(tiny);
        let slices: [&[u8]; 1] = [&picture];
        let unit = unit(0, FrameKind::Delta, &slices);
        assert!(!ReedSolomonEncoder::supports(60_000, 12_000));

        let mut out = Vec::new();
        packetiser.packetise(&unit, &mut out);
        assert_eq!(
            out.len(),
            60_000,
            "the picture goes out, the parity does not"
        );
        assert_eq!(packetiser.datagrams(&unit), 60_000);
    }

    #[test]
    fn a_multi_slice_frame_carries_no_parity() {
        let a = picture(300, 20);
        let b = picture(200, 21);
        let slices: [&[u8]; 2] = [&a, &b];
        let mut packetiser = Packetiser::new(PAYLOAD);
        let unit = unit(0, FrameKind::Key, &slices);
        let mut out = Vec::new();
        packetiser.packetise(&unit, &mut out);
        assert_eq!(
            out.len(),
            unit.datagrams(PAYLOAD),
            "parity indexes a frame's datagrams in send order, which two slices make unknowable"
        );
    }

    #[test]
    fn an_odd_payload_is_rounded_down_to_a_shard_the_coder_takes() {
        let odd = NonZeroUsize::new(65).expect("nonzero");
        let packetiser = Packetiser::new(odd);
        assert_eq!(packetiser.payload_len().get(), 64);
        let one = NonZeroUsize::new(1).expect("nonzero");
        assert_eq!(Packetiser::new(one).payload_len().get(), 1);
    }

    #[test]
    fn parity_is_sized_against_the_frame_and_the_loss() {
        let parity = Parity::fixed(20);
        assert_eq!(parity.shards(FrameKind::Delta, 12), 3);
        assert_eq!(parity.shards(FrameKind::Key, 92), 28);
        // A frame of one or two datagrams still gets a shard. It is the frame
        // a percentage rounds away, and at sixty a second an unprotected one is
        // a broken reference chain more than once a second.
        assert_eq!(parity.shards(FrameKind::Delta, 1), 1);
        assert_eq!(Parity::fixed(0).shards(FrameKind::Key, 92), 0);
    }

    #[test]
    fn the_parity_level_follows_loss_within_a_floor_and_a_ceiling() {
        let mut parity = Parity::fixed(0);
        parity.observe(0, 1000);
        assert_eq!(
            parity.level(),
            Parity::FLOOR,
            "a clean link still carries the floor, because loss is measured after the fact"
        );

        // The 2.79% measured on the wireless link asks for the 20% the Monte
        // Carlo was run at.
        parity.observe(279, 9721);
        assert_eq!(parity.level(), Parity::MEASURED);

        parity.observe(5000, 5000);
        assert_eq!(
            parity.level(),
            Parity::CEILING,
            "half a link lost is the cap"
        );
    }

    #[test]
    fn the_parity_level_rises_at_once_and_falls_only_once_the_reading_holds() {
        let mut parity = Parity::fixed(Parity::FLOOR);
        parity.observe(279, 9721);
        assert_eq!(
            parity.level(),
            Parity::MEASURED,
            "one report is enough to rise"
        );

        for report in 0..3 {
            parity.observe(0, 1000);
            assert_eq!(
                parity.level(),
                Parity::MEASURED,
                "report {report} is not yet enough to fall"
            );
        }
        parity.observe(0, 1000);
        assert_eq!(parity.level(), Parity::FLOOR);

        // Inside the dead band nothing moves, however long it holds.
        let mut steady = Parity::fixed(20);
        for _ in 0..20 {
            steady.observe(250, 9750);
        }
        assert_eq!(steady.level(), 20);
    }

    /// A deterministic link at the loss rate measured on the real wireless
    /// path, with and without parity.
    ///
    /// The thresholds below are **correctness thresholds, not a measurement**.
    /// The run is a few hundred frames, which is long enough to show that the
    /// two schemes are on opposite sides of usable and far too short to put a
    /// figure on either. `spec/findings.md` holds the measurement.
    #[test]
    fn parity_carries_a_stream_the_measured_loss_rate_otherwise_destroys() {
        let protected = stream(Parity::fixed(Parity::MEASURED));
        let bare = stream(Parity::fixed(0));
        assert!(
            protected.shown * 100 > protected.offered * 99,
            "20% parity must carry the stream: {} of {} frames",
            protected.shown,
            protected.offered
        );
        assert!(
            bare.shown * 4 < bare.offered,
            "without parity the same link must be visibly unusable: {} of {} frames",
            bare.shown,
            bare.offered
        );
        assert!(
            protected.wire < bare.wire,
            "parity must cost less on the wire than the key frames it replaces: \
             {} against {} bytes",
            protected.wire,
            bare.wire
        );
    }

    struct Run {
        offered: u64,
        shown: u64,
        wire: u64,
    }

    /// Frames a 60 Hz stream offers in the run above. Four seconds.
    const FRAMES: u32 = 240;

    /// Frames between a key frame request and the key frame that answers it,
    /// which is the 250 ms budget in `idr.rs` at 60 Hz.
    const KEY_DELAY: u32 = 15;

    /// The loss measured on the wireless link, in parts per ten thousand.
    const LOSS: u64 = 279;

    /// Datagrams a 1080p key frame and a P frame occupied on the target
    /// machine. The pictures below are scaled to the same datagram counts at a
    /// test-sized payload, because what decides survival is the count and not
    /// the bytes.
    const KEY_DATAGRAMS: usize = 92;
    const DELTA_DATAGRAMS: usize = 12;

    fn stream(parity: Parity) -> Run {
        let mut packetiser = Packetiser::new(PAYLOAD);
        packetiser.set_parity(parity);
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        let mut link = Link {
            state: 0x9E37_79B9_7F4A_7C15,
        };
        let mut now = Instant::now();
        let period = Duration::from_micros(16_667);

        let key_picture = picture(KEY_DATAGRAMS * PAYLOAD.get(), 22);
        let delta_picture = picture(DELTA_DATAGRAMS * PAYLOAD.get(), 23);
        let mut answer_at: Option<u32> = None;
        let mut run = Run {
            offered: u64::from(FRAMES),
            shown: 0,
            wire: 0,
        };
        let mut out = Vec::new();
        for id in 0..FRAMES {
            let key = id == 0 || answer_at == Some(id);
            if key {
                answer_at = None;
            }
            let slice: &[u8] = if key { &key_picture } else { &delta_picture };
            let kind = if key {
                FrameKind::Key
            } else {
                FrameKind::Delta
            };
            out.clear();
            packetiser.packetise(&unit(id, kind, &[slice]), &mut out);
            for datagram in &out {
                run.wire += datagram.len() as u64;
                if link.drops() {
                    continue;
                }
                reassembler.push(datagram, now).expect("well formed");
                run.shown += drain(&mut reassembler).len() as u64;
            }
            now += period;
            reassembler.expire(now);
            run.shown += drain(&mut reassembler).len() as u64;
            if reassembler.needs_keyframe() && answer_at.is_none() {
                answer_at = Some(id + 1 + KEY_DELAY);
            }
        }
        run
    }

    /// A deterministic link. `xorshift64*`, so the loss pattern is the same on
    /// every machine and every run.
    struct Link {
        state: u64,
    }

    impl Link {
        fn drops(&mut self) -> bool {
            let mut x = self.state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.state = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D) % 10_000 < LOSS
        }
    }
}
