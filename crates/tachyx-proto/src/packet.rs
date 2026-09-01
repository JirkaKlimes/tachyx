//! Access units to datagrams, and datagrams back to access units.
//!
//! Both directions live here rather than in the transport because they are pure
//! functions over the sixteen-byte header of [`crate::media`], and because the
//! client reassembles video without wanting a QUIC server linked in to do it.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::num::{NonZeroU16, NonZeroUsize};
use std::time::{Duration, Instant};

use crate::media::{
    Fragment, FrameId, FrameKind, HEADER_LEN, MalformedPacket, PacketHeader, Seq, Timestamp,
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

    /// Datagrams this picture occupies at `payload_len` bytes of video each.
    ///
    /// Fragmentation is per slice, so this is not `encoded_len` divided by
    /// `payload_len` once a picture has more than one slice.
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

/// Divides access units into datagrams and stamps the session-wide counters.
///
/// One per connection, on the host. The sequence number it writes is what lets
/// a receiver see a gap across a frame boundary, so it must not be restarted
/// for each frame.
#[derive(Debug)]
pub struct Packetiser {
    payload_len: NonZeroUsize,
    seq: Seq,
}

impl Packetiser {
    /// Starts a session that puts `payload_len` bytes of video in each datagram.
    #[must_use]
    pub const fn new(payload_len: NonZeroUsize) -> Self {
        Self {
            payload_len,
            seq: Seq(0),
        }
    }

    /// Bytes of video each datagram carries.
    #[must_use]
    pub const fn payload_len(&self) -> NonZeroUsize {
        self.payload_len
    }

    /// Resizes datagrams after the path maximum transmission unit changed.
    ///
    /// The sequence number carries on, because it counts datagrams rather than
    /// bytes and the receiver's gap detection depends on it being unbroken.
    pub const fn set_payload_len(&mut self, payload_len: NonZeroUsize) {
        self.payload_len = payload_len;
    }

    /// Appends every datagram of `unit` to `out`, in send order.
    ///
    /// Each datagram is a header followed by its share of one slice. A slice
    /// never shares a datagram with another, so a lost fragment costs one slice
    /// rather than the picture.
    pub fn packetise(&mut self, unit: &AccessUnit<'_>, out: &mut Vec<Vec<u8>>) {
        let last = unit.slices.len() - 1;
        // The counter runs beside the slices rather than being cast from an
        // index, because `MAX_SLICES` guarantees it fits and a cast would not
        // say so.
        for (slice, id) in unit.slices.iter().zip(0u8..) {
            for (fragment, range) in Fragment::split(slice.len(), self.payload_len) {
                let header = PacketHeader {
                    frame: unit.id,
                    capture: unit.capture,
                    seq: self.seq,
                    fragment,
                    slice: id,
                    kind: unit.kind,
                    last_slice: usize::from(id) == last,
                };
                self.seq = self.seq.next();
                let mut datagram = Vec::with_capacity(HEADER_LEN + range.len());
                datagram.extend_from_slice(&header.to_bytes());
                datagram.extend_from_slice(&slice[range]);
                out.push(datagram);
            }
        }
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
/// that was merely reordered, and a reordered packet on a link whose round trip
/// is under a millisecond arrives immediately. Waiting this long after the
/// signal keeps the frame that reordering would otherwise have cost, and is
/// still a small fraction of the eight to sixteen milliseconds a frame period
/// gives.
pub const REORDER_WINDOW: Duration = Duration::from_millis(2);

/// What a reassembler has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    /// Datagrams parsed.
    pub packets_received: u64,
    /// Datagrams the sequence number says never arrived.
    pub packets_lost: u64,
    /// Datagrams for a frame that had already been decided.
    pub packets_late: u64,
    /// Frames handed to the decoder.
    pub frames_delivered: u64,
    /// Frames abandoned with fragments missing.
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
#[derive(Debug)]
pub struct Reassembler {
    deadline: Duration,
    open: BTreeMap<u32, Partial>,
    previous: Option<Marker>,
    /// Frames below this have been delivered, dropped, or passed over. Their
    /// packets are late rather than useful.
    floor: u32,
    needs_key: bool,
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
}

/// Fragments of one slice, keyed by index so that nothing is allocated for a
/// fragment that has not arrived and a hostile count cannot be amplified into
/// memory.
#[derive(Debug)]
struct PartialSlice {
    count: NonZeroU16,
    fragments: BTreeMap<u16, Vec<u8>>,
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
            previous: None,
            floor: 0,
            // Nothing before the first key frame can be decoded.
            needs_key: true,
            stats: ReassemblyStats::default(),
        }
    }

    /// Whether the decoder cannot continue until a key frame arrives.
    ///
    /// Set by every dropped frame and cleared by the next complete key frame.
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

    /// When the earliest open frame gives up, if any frame is open.
    ///
    /// A receiver waits on this rather than on a fixed tick, so a session
    /// watching a static screen costs no wakeups.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.open.values().map(|partial| partial.deadline).min()
    }

    /// Takes one datagram, returning the access unit it completed.
    ///
    /// Runs [`Reassembler::expire`] first, so a caller that keeps feeding
    /// datagrams needs no timer of its own.
    ///
    /// # Errors
    /// If the datagram is not a video packet.
    pub fn push(
        &mut self,
        datagram: &[u8],
        now: Instant,
    ) -> Result<Option<Frame>, MalformedPacket> {
        self.expire(now);
        let (header, payload) = PacketHeader::parse(datagram)?;
        self.stats.packets_received += 1;

        let gap_within_frame = self.track_sequence(&header);
        if header.frame.0 < self.floor {
            self.stats.packets_late += 1;
            return Ok(None);
        }
        if header.frame.0 > self.floor {
            for partial in self.open.range_mut(..header.frame.0).map(|(_, v)| v) {
                partial.deadline = partial.deadline.min(now + REORDER_WINDOW);
            }
        }

        let mut open = match self.open.entry(header.frame.0) {
            Entry::Occupied(frame) => frame,
            Entry::Vacant(slot) => slot.insert_entry(Partial {
                capture: header.capture,
                kind: header.kind,
                deadline: now + self.deadline,
                last_slice: None,
                slices: BTreeMap::new(),
            }),
        };
        let partial = open.get_mut();
        if gap_within_frame {
            // The missing packets sit between two packets of this frame, so
            // they were this frame's. The packet in hand is still kept: it may
            // be the reordered one that fills the hole.
            partial.deadline = partial.deadline.min(now + REORDER_WINDOW);
        }
        if header.last_slice {
            partial.last_slice = Some(header.slice);
        }
        // A fragment count that disagrees with the one an earlier packet of the
        // same slice carried is a sender fault. The first count wins and the
        // frame times out if that leaves it unfinishable.
        partial
            .slices
            .entry(header.slice)
            .or_insert_with(|| PartialSlice {
                count: header.fragment.count(),
                fragments: BTreeMap::new(),
            })
            .fragments
            .entry(header.fragment.index())
            .or_insert_with(|| payload.to_vec());
        if !partial.complete() {
            return Ok(None);
        }

        let partial = open.remove();
        if self.abandon_below(header.frame.0) > 0 {
            self.needs_key = true;
        }
        self.floor = header.frame.0 + 1;
        if matches!(header.kind, FrameKind::Key) {
            self.needs_key = false;
        } else if self.needs_key {
            self.stats.frames_undecodable += 1;
            return Ok(None);
        }
        self.stats.frames_delivered += 1;
        Ok(Some(partial.into_frame(header.frame)))
    }

    /// Drops frames whose wait has run out.
    ///
    /// Only needed on a connection where no datagram is arriving to run it, so
    /// on a timer beside the receive loop rather than in it.
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
        self.abandon_below(highest + 1);
        self.needs_key = true;
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

    /// Decides every frame below `id`, which nothing can complete any more, and
    /// reports how many frames that cost.
    ///
    /// The frames still open are the damaged ones. The rest of the span is
    /// frames of which not one packet arrived, which nothing but the gap in the
    /// identifiers reveals.
    fn abandon_below(&mut self, id: u32) -> u64 {
        let span = u64::from(id.saturating_sub(self.floor));
        if span == 0 {
            return 0;
        }
        let damaged: Vec<u32> = self.open.range(..id).map(|(&open, _)| open).collect();
        for open in &damaged {
            self.open.remove(open);
        }
        let damaged = damaged.len() as u64;
        self.stats.frames_dropped += damaged;
        self.stats.frames_missing += span - damaged;
        self.floor = id;
        span
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

    fn deliver(reassembler: &mut Reassembler, datagrams: &[Vec<u8>], now: Instant) -> Vec<Frame> {
        datagrams
            .iter()
            .filter_map(|datagram| reassembler.push(datagram, now).expect("well formed"))
            .collect()
    }

    #[test]
    fn a_single_slice_frame_survives_the_round_trip() {
        let picture = picture(1000, 0x5A);
        let mut packetiser = Packetiser::new(PAYLOAD);
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
    fn slices_are_concatenated_in_decode_order() {
        let parameter_sets_with_first_slice = picture(300, 1);
        let second = picture(200, 2);
        let slices: [&[u8]; 2] = [&parameter_sets_with_first_slice, &second];
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
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
            assert!(reassembler.push(datagram, start).unwrap().is_none());
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
        let mut packetiser = Packetiser::new(PAYLOAD);
        let picture = picture(600, 8);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);

        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        for datagram in &out[..out.len() - 1] {
            assert!(reassembler.push(datagram, start).unwrap().is_none());
        }
        reassembler.expire(start + Duration::from_millis(49));
        assert_eq!(reassembler.stats().frames_dropped, 0);
        reassembler.expire(start + Duration::from_millis(51));
        assert_eq!(reassembler.stats().frames_dropped, 1);
        assert!(reassembler.needs_keyframe());
    }

    #[test]
    fn a_delta_frame_is_thrown_away_until_a_key_frame_resynchronises() {
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
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
        let mut packetiser = Packetiser::new(PAYLOAD);
        let picture = picture(200, 11);
        let mut out = Vec::new();
        packetiser.packetise(&unit(0, FrameKind::Key, &[&picture]), &mut out);
        let start = Instant::now();
        let mut reassembler = Reassembler::new(Duration::from_millis(50));
        assert_eq!(deliver(&mut reassembler, &out, start).len(), 1);
        assert!(reassembler.push(&out[0], start).unwrap().is_none());
        assert_eq!(reassembler.stats().packets_late, 1);
        assert_eq!(reassembler.stats().frames_delivered, 1);
    }

    #[test]
    fn induced_loss_and_reordering_never_yields_a_corrupt_frame() {
        let mut packetiser = Packetiser::new(PAYLOAD);
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
            if let Some(frame) = reassembler.push(datagram, start).expect("well formed") {
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

        let mut packetiser = Packetiser::new(PAYLOAD);
        let mut out = Vec::new();
        packetiser.packetise(&unit, &mut out);
        assert_eq!(out.len(), unit.datagrams(PAYLOAD));
    }
}
