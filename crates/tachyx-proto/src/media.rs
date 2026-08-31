//! The video datagram header.
//!
//! Sixteen bytes, little-endian, written and read by hand. This is the only part
//! of the protocol on the per-packet path, and the part most worth auditing, so
//! it is specified as bytes rather than derived from a Rust type.
//!
//! There is no RTP framing, no synchronisation source, and no per-packet
//! authentication tag. QUIC already supplies encryption, connection identity and
//! path management. Dropping RTP is what buys the room for a real timestamp.

use core::num::NonZeroU16;

/// Length of a packet header in bytes.
pub const HEADER_LEN: usize = 16;

/// A capture instant, relative to the start of the session, in units of 100
/// microseconds.
///
/// Microseconds would overflow a `u32` after 71 minutes, and a remote desktop
/// session routinely runs longer than that. At 100-microsecond resolution the
/// same four bytes last 119 hours, which removes the wraparound entirely rather
/// than asking every receiver to unwrap it correctly.
///
/// The resolution is finer than anything that consumes it: latency is reported in
/// milliseconds, and congestion feedback measures arrival deltas in units of 250
/// microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u32);

impl Timestamp {
    /// The number of microseconds in one tick.
    const TICK_US: u64 = 100;

    /// Quantises a session-relative duration to a timestamp, saturating at the
    /// 119-hour ceiling rather than wrapping.
    #[must_use]
    pub fn from_session_micros(micros: u64) -> Self {
        Self(u32::try_from(micros / Self::TICK_US).unwrap_or(u32::MAX))
    }

    /// The instant in microseconds since the session began.
    #[must_use]
    pub fn as_session_micros(self) -> u64 {
        u64::from(self.0) * Self::TICK_US
    }
}

/// Which packet of a slice this is.
///
/// The pair is one type because the invariant that binds them, that the index
/// falls inside the count, has to hold for the receiver to size a reassembly
/// buffer. Constructing one out of range is impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    count: NonZeroU16,
    index: u16,
}

impl Fragment {
    /// Describes packet `index` of `count`.
    ///
    /// Returns `None` unless the index falls inside the count.
    #[must_use]
    pub const fn new(index: u16, count: NonZeroU16) -> Option<Self> {
        if index < count.get() {
            Some(Self { count, index })
        } else {
            None
        }
    }

    /// Total packets in this slice.
    ///
    /// Every packet carries it, so any one of them can size the reassembly
    /// buffer without waiting for a particular packet to arrive.
    #[must_use]
    pub const fn count(self) -> NonZeroU16 {
        self.count
    }

    /// This packet's position within the slice.
    #[must_use]
    pub const fn index(self) -> u16 {
        self.index
    }

    /// Whether this is the last packet of the slice.
    #[must_use]
    pub const fn is_last(self) -> bool {
        self.index + 1 == self.count.get()
    }
}

/// Whether a frame can be decoded without any frame before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Decodable on its own. A receiver that has lost synchronisation discards
    /// everything until one of these arrives.
    Key,
    /// Depends on earlier frames.
    Delta,
}

/// A datagram sequence number, counted over the whole session rather than per
/// frame, so a gap is detectable across a frame boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seq(pub u16);

impl Seq {
    /// The next sequence number, wrapping.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// How far `self` is ahead of `other`, treating the space as circular in the
    /// manner of RFC 1982.
    ///
    /// Negative means `self` arrived out of order. Comparing directly with `<`
    /// would call sequence number 0 older than 65535 rather than newer.
    #[must_use]
    pub fn distance_from(self, other: Self) -> i32 {
        let ahead = i32::from(self.0.wrapping_sub(other.0));
        if ahead < 0x8000 {
            ahead
        } else {
            ahead - 0x1_0000
        }
    }
}

/// A frame number, counted over the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameId(pub u32);

/// The header carried by every video datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Which frame this packet belongs to. The reassembly key, and what a
    /// recovery request names.
    pub frame: FrameId,
    /// When the frame was captured.
    pub capture: Timestamp,
    /// Position in the session-wide datagram sequence.
    pub seq: Seq,
    /// Which packet of the slice this is.
    pub fragment: Fragment,
    /// Which slice of the frame this packet belongs to.
    ///
    /// Slices are independently decodable, so losing one costs a band of the
    /// picture rather than the whole frame. There is deliberately no field
    /// counting the slices in a frame: `last_slice` supplies that, which lets the
    /// host send slice zero while slice one is still being encoded.
    pub slice: u8,
    /// Whether the frame stands alone.
    pub kind: FrameKind,
    /// Whether this is the final slice of the frame.
    pub last_slice: bool,
}

impl PacketHeader {
    const FLAG_KEY: u8 = 1 << 0;
    const FLAG_LAST_SLICE: u8 = 1 << 1;

    /// Writes the header into the first [`HEADER_LEN`] bytes of `out`.
    ///
    /// # Panics
    /// If `out` is shorter than [`HEADER_LEN`].
    pub fn write(self, out: &mut [u8]) {
        let (head, _) = out.split_at_mut(HEADER_LEN);
        let mut flags = 0;
        if matches!(self.kind, FrameKind::Key) {
            flags |= Self::FLAG_KEY;
        }
        if self.last_slice {
            flags |= Self::FLAG_LAST_SLICE;
        }
        head[0..4].copy_from_slice(&self.frame.0.to_le_bytes());
        head[4..8].copy_from_slice(&self.capture.0.to_le_bytes());
        head[8..10].copy_from_slice(&self.seq.0.to_le_bytes());
        head[10..12].copy_from_slice(&self.fragment.count.get().to_le_bytes());
        head[12..14].copy_from_slice(&self.fragment.index.to_le_bytes());
        head[14] = self.slice;
        head[15] = flags;
    }

    /// Splits a received datagram into its header and its video payload.
    ///
    /// Unknown flag bits are ignored rather than rejected, so a later version can
    /// use them without this version discarding the packet.
    ///
    /// # Errors
    /// If the datagram is too short to hold a header, carries no payload, or
    /// describes a fragment whose index falls outside its count.
    pub fn parse(datagram: &[u8]) -> Result<(Self, &[u8]), MalformedPacket> {
        let (head, payload) = datagram
            .split_at_checked(HEADER_LEN)
            .ok_or(MalformedPacket::TooShort)?;
        if payload.is_empty() {
            return Err(MalformedPacket::NoPayload);
        }
        let count = NonZeroU16::new(u16::from_le_bytes([head[10], head[11]]))
            .ok_or(MalformedPacket::EmptySlice)?;
        let index = u16::from_le_bytes([head[12], head[13]]);
        let fragment = Fragment::new(index, count).ok_or(MalformedPacket::IndexOutOfRange)?;
        let flags = head[15];
        Ok((
            Self {
                frame: FrameId(u32::from_le_bytes([head[0], head[1], head[2], head[3]])),
                capture: Timestamp(u32::from_le_bytes([head[4], head[5], head[6], head[7]])),
                seq: Seq(u16::from_le_bytes([head[8], head[9]])),
                fragment,
                slice: head[14],
                kind: if flags & Self::FLAG_KEY == 0 {
                    FrameKind::Delta
                } else {
                    FrameKind::Key
                },
                last_slice: flags & Self::FLAG_LAST_SLICE != 0,
            },
            payload,
        ))
    }
}

/// Why a received datagram could not be a video packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MalformedPacket {
    /// Shorter than a header.
    #[error("datagram is shorter than a {HEADER_LEN}-byte header")]
    TooShort,
    /// Header but no video bytes.
    #[error("datagram carries a header and no payload")]
    NoPayload,
    /// A slice claiming to contain no packets, which cannot describe this one.
    #[error("slice claims to hold no packets")]
    EmptySlice,
    /// A packet index outside the slice it claims to belong to.
    #[error("packet index falls outside its slice")]
    IndexOutOfRange,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> PacketHeader {
        PacketHeader {
            frame: FrameId(0x0123_4567),
            capture: Timestamp::from_session_micros(9_999_900),
            seq: Seq(0xBEEF),
            fragment: Fragment::new(2, NonZeroU16::new(7).unwrap()).unwrap(),
            slice: 3,
            kind: FrameKind::Key,
            last_slice: true,
        }
    }

    #[test]
    fn header_survives_a_round_trip() {
        let mut buf = [0u8; HEADER_LEN + 4];
        header().write(&mut buf);
        buf[HEADER_LEN..].copy_from_slice(b"data");
        let (parsed, payload) = PacketHeader::parse(&buf).unwrap();
        assert_eq!(parsed, header());
        assert_eq!(payload, b"data");
    }

    #[test]
    fn header_occupies_exactly_sixteen_bytes() {
        let mut buf = [0xAAu8; HEADER_LEN + 1];
        header().write(&mut buf);
        assert_eq!(buf[HEADER_LEN], 0xAA, "write overran the header");
    }

    #[test]
    fn unknown_flag_bits_do_not_reject_a_packet() {
        let mut buf = [0u8; HEADER_LEN + 1];
        header().write(&mut buf);
        buf[15] |= 0b1111_1100;
        buf[HEADER_LEN] = 1;
        let (parsed, _) = PacketHeader::parse(&buf).expect("later versions may set spare bits");
        assert_eq!(parsed.kind, FrameKind::Key);
        assert!(parsed.last_slice);
    }

    #[test]
    fn a_fragment_index_outside_its_count_cannot_be_built() {
        assert!(Fragment::new(7, NonZeroU16::new(7).unwrap()).is_none());
        assert!(Fragment::new(6, NonZeroU16::new(7).unwrap()).is_some());
    }

    #[test]
    fn malformed_packets_are_named() {
        let mut buf = [0u8; HEADER_LEN + 1];
        header().write(&mut buf);
        assert_eq!(
            PacketHeader::parse(&buf[..HEADER_LEN - 1]),
            Err(MalformedPacket::TooShort)
        );
        assert_eq!(
            PacketHeader::parse(&buf[..HEADER_LEN]),
            Err(MalformedPacket::NoPayload)
        );
        let mut empty_slice = buf;
        empty_slice[10..12].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            PacketHeader::parse(&empty_slice),
            Err(MalformedPacket::EmptySlice)
        );
        let mut past_end = buf;
        past_end[12..14].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            PacketHeader::parse(&past_end),
            Err(MalformedPacket::IndexOutOfRange)
        );
    }

    #[test]
    fn sequence_comparison_survives_the_wrap() {
        assert_eq!(Seq(0).distance_from(Seq(65535)), 1);
        assert_eq!(Seq(65535).distance_from(Seq(0)), -1);
        assert_eq!(Seq(10).distance_from(Seq(4)), 6);
        assert_eq!(Seq(4).distance_from(Seq(10)), -6);
    }

    #[test]
    fn a_timestamp_spans_a_session_longer_than_any_desktop_stays_open() {
        let ceiling = Timestamp::from_session_micros(u64::from(u32::MAX) * 100);
        assert!(ceiling.as_session_micros() / 1_000_000 / 3600 > 100);
        assert_eq!(
            Timestamp::from_session_micros(12_345_600).as_session_micros(),
            12_345_600
        );
    }
}
