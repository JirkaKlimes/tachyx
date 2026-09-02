//! The AV1 reader: one temporal unit of open bitstream units in, [`Unit`] out.
//!
//! The host sends the low overhead bitstream format, which is what the
//! GStreamer caps `stream-format=obu-stream` name: each unit is a one or two
//! byte header, a LEB128 size, and a payload, with no start codes and no
//! emulation prevention. The sequence header travels in the same temporal unit
//! as every key frame, so the coded size is read the way the H.264 reader reads
//! it out of a sequence parameter set.
//!
//! A frame is a key frame when its frame header says `KEY_FRAME`. The one place
//! that header cannot be read on its own is a reduced still picture stream,
//! whose frame header states nothing because every picture is a key picture;
//! its sequence header says so, and it travels in every temporal unit of such a
//! stream, so the flag is always in hand when it matters.

use super::{Coded, Unit};

/// A sequence header.
const OBU_SEQUENCE_HEADER: u8 = 1;

/// A frame header without its picture, which a tile group then carries.
const OBU_FRAME_HEADER: u8 = 3;

/// A frame header and its picture in one unit.
const OBU_FRAME: u8 = 6;

/// Reads what one temporal unit contains.
///
/// This walks the unit headers, skipping each payload by its stated size, and
/// stops at the first frame: the sequence header comes before it and the
/// picture is the end of the temporal unit, so nothing past there can change
/// either answer. A unit whose sizes do not parse ends the walk with whatever
/// was read before it.
pub(super) fn unit(bytes: &[u8]) -> Unit {
    let mut unit = Unit {
        coded: None,
        key: false,
    };
    let mut reduced = false;
    for (kind, payload) in Obus::of(bytes) {
        match kind {
            OBU_SEQUENCE_HEADER => {
                if let Some(sequence) = sequence(payload) {
                    unit.coded = Some(sequence.coded);
                    reduced = sequence.reduced;
                }
            }
            OBU_FRAME | OBU_FRAME_HEADER => {
                unit.key = reduced || key(payload).unwrap_or(false);
                break;
            }
            _ => {}
        }
    }
    unit
}

/// What the sequence header declares.
struct Sequence {
    /// The largest frame size of the sequence, which is the size the host's
    /// encoder codes every picture at and the size the decoder writes.
    coded: Coded,
    /// Whether this is a reduced still picture stream, in which every frame
    /// header is empty and every picture is a key picture.
    reduced: bool,
}

/// Reads a sequence header payload up to the frame size and no further.
///
/// Everything before the size has to be read to reach it: the timing and
/// decoder model information and the operating points stand between the profile
/// and the size, and all of them are stepped over field by field.
fn sequence(payload: &[u8]) -> Option<Sequence> {
    let mut bits = Bits::of(payload);
    bits.f(3)?;
    bits.f(1)?;
    let reduced = bits.f(1)? == 1;
    if reduced {
        bits.f(5)?;
    } else {
        let mut model = false;
        let mut delay_bits = 0;
        if bits.f(1)? == 1 {
            bits.f(32)?;
            bits.f(32)?;
            if bits.f(1)? == 1 {
                bits.uvlc()?;
            }
            model = bits.f(1)? == 1;
            if model {
                delay_bits = bits.f(5)? + 1;
                bits.f(32)?;
                bits.f(5)?;
                bits.f(5)?;
            }
        }
        let delays = bits.f(1)? == 1;
        let points = bits.f(5)? + 1;
        for _ in 0..points {
            bits.f(12)?;
            if bits.f(5)? > 7 {
                bits.f(1)?;
            }
            if model && bits.f(1)? == 1 {
                bits.f(delay_bits)?;
                bits.f(delay_bits)?;
                bits.f(1)?;
            }
            if delays && bits.f(1)? == 1 {
                bits.f(4)?;
            }
        }
    }
    let width_bits = bits.f(4)? + 1;
    let height_bits = bits.f(4)? + 1;
    Some(Sequence {
        coded: Coded {
            width: u16::try_from(bits.f(width_bits)?.checked_add(1)?).ok()?,
            height: u16::try_from(bits.f(height_bits)?.checked_add(1)?).ok()?,
        },
        reduced,
    })
}

/// Whether a frame header that states its own type states `KEY_FRAME`.
///
/// A frame that shows an already decoded picture is not one: it carries no
/// picture at all, so it is nowhere a decoder can start.
fn key(payload: &[u8]) -> Option<bool> {
    let mut bits = Bits::of(payload);
    if bits.f(1)? == 1 {
        return Some(false);
    }
    Some(bits.f(2)? == 0)
}

/// The open bitstream units of one temporal unit, as header type and payload.
struct Obus<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Obus<'a> {
    fn of(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
}

impl<'a> Iterator for Obus<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let header = *self.bytes.get(self.at)?;
        // The forbidden bit. Set, this is not an open bitstream unit and
        // nothing after it can be found.
        if header & 0x80 != 0 {
            return None;
        }
        let mut at = self.at + 1;
        // The extension header, a temporal and a spatial identifier this
        // single-layer stream never sets but the format allows.
        if header & 0x04 != 0 {
            at += 1;
        }
        let end = if header & 0x02 != 0 {
            let (size, after) = leb128(self.bytes, at)?;
            at = after;
            at.checked_add(size)?
        } else {
            // A unit without a size field runs to the end of the temporal
            // unit, which the format allows only for the last unit.
            self.bytes.len()
        };
        let payload = self.bytes.get(at..end)?;
        self.at = end;
        Some(((header >> 3) & 0x0f, payload))
    }
}

/// A little endian base 128 size, returned with the position after it.
///
/// The specification caps it at eight bytes, and past there the walk is a
/// hostile stream's to choose the length of.
fn leb128(bytes: &[u8], mut at: usize) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    for shift in 0..8u32 {
        let byte = *bytes.get(at)?;
        value |= u64::from(byte & 0x7f) << (7 * shift);
        at += 1;
        if byte & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, at));
        }
    }
    None
}

/// A reader over a payload, in the order the syntax is written.
///
/// Nothing is escaped in this format, so this is plain bit arithmetic.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn of(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// A fixed width field of up to 32 bits.
    fn f(&mut self, count: u32) -> Option<u32> {
        let mut value: u64 = 0;
        for _ in 0..count {
            let byte = *self.bytes.get(self.at / 8)?;
            value = (value << 1) | u64::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        u32::try_from(value).ok()
    }

    /// A variable length unsigned code, used by the timing information.
    fn uvlc(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.f(1)? == 0 {
            leading += 1;
            // No field read here is this wide, so past there the payload is
            // not a sequence header.
            if leading > 31 {
                return None;
            }
        }
        Some((1u32 << leading) - 1 + self.f(leading)?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Coded;
    use super::unit;

    /// A temporal delimiter, which begins every temporal unit the host sends.
    const TEMPORAL_DELIMITER: &[u8] = &[0x12, 0x00];

    /// A 1920x1082 sequence header off a VA encoder, the same driver family as
    /// the host's own.
    const SEQ_VA_1920X1082: &[u8] = &[
        0x0a, 0x0b, 0x00, 0x00, 0x00, 0x4a, 0xab, 0xbf, 0xc3, 0x90, 0x08, 0x70, 0x01,
    ];

    /// A 2580x1080 sequence header off libaom, which fills the operating point
    /// block differently.
    const SEQ_AOM_2580X1080: &[u8] = &[
        0x0a, 0x0c, 0x00, 0x00, 0x00, 0x62, 0xea, 0x84, 0xe1, 0xb9, 0x95, 0xf2, 0x00, 0x80,
    ];

    /// A 3440x1440 sequence header off SVT-AV1, a third shape of the same
    /// block.
    const SEQ_SVT_3440X1440: &[u8] = &[
        0x0a, 0x0c, 0x00, 0x00, 0x00, 0x6a, 0xeb, 0x5b, 0xec, 0xf8, 0x95, 0xf2, 0x00, 0x80,
    ];

    /// The frame header of a key frame, off the same VA encoder.
    const KEY_HEADER_VA: &[u8] = &[0x1a, 0x08, 0x10, 0x00, 0x82, 0x80, 0x02, 0x08, 0x20, 0x81];

    /// The frame header of the picture after it, which refers back to it.
    const DELTA_HEADER_VA: &[u8] = &[
        0x1a, 0x0e, 0x30, 0x02, 0x00, 0x80, 0x00, 0x00, 0x24, 0x19, 0x00, 0x10, 0x41, 0x04, 0x20,
        0x08,
    ];

    /// A key frame unit off libaom, header and picture in one, cut to its
    /// first payload bytes with the size field rewritten to match.
    const KEY_FRAME_AOM: &[u8] = &[0x32, 0x04, 0x10, 0x00, 0x8f, 0x00];

    /// The delta frame unit after it, cut the same way.
    const DELTA_FRAME_AOM: &[u8] = &[0x32, 0x04, 0x30, 0x03, 0xc0, 0x80];

    fn temporal_unit(obus: &[&[u8]]) -> Vec<u8> {
        obus.concat()
    }

    #[test]
    fn a_sequence_header_states_the_size_the_decoder_writes() {
        for (sequence, width, height) in [
            (SEQ_VA_1920X1082, 1920, 1082),
            (SEQ_AOM_2580X1080, 2580, 1080),
            (SEQ_SVT_3440X1440, 3440, 1440),
        ] {
            let stream = temporal_unit(&[TEMPORAL_DELIMITER, sequence, KEY_HEADER_VA]);
            assert_eq!(unit(&stream).coded, Some(Coded { width, height }));
        }
    }

    #[test]
    fn a_key_frame_is_the_sequence_header_and_a_key_picture() {
        let stream = temporal_unit(&[TEMPORAL_DELIMITER, SEQ_VA_1920X1082, KEY_HEADER_VA]);
        let read = unit(&stream);
        assert!(read.key);
        assert!(read.coded.is_some());
    }

    #[test]
    fn a_later_frame_carries_neither() {
        let stream = temporal_unit(&[TEMPORAL_DELIMITER, DELTA_HEADER_VA]);
        let read = unit(&stream);
        assert!(!read.key);
        assert_eq!(read.coded, None);
    }

    #[test]
    fn a_frame_that_carries_its_picture_reads_the_same_as_a_bare_header() {
        assert!(unit(&temporal_unit(&[TEMPORAL_DELIMITER, KEY_FRAME_AOM])).key);
        assert!(!unit(&temporal_unit(&[TEMPORAL_DELIMITER, DELTA_FRAME_AOM])).key);
    }

    #[test]
    fn a_temporal_unit_needs_no_delimiter_to_be_read() {
        let stream = temporal_unit(&[SEQ_VA_1920X1082, KEY_HEADER_VA]);
        let read = unit(&stream);
        assert!(read.key);
        assert!(read.coded.is_some());
    }

    #[test]
    fn a_frame_that_shows_an_already_decoded_picture_is_not_a_key() {
        // `show_existing_frame` set: the first payload bit is one.
        let stream = temporal_unit(&[TEMPORAL_DELIMITER, &[0x1a, 0x01, 0x80]]);
        assert!(!unit(&stream).key);
    }

    #[test]
    fn a_reduced_still_picture_stream_is_key_pictures_throughout() {
        // Hand written, because no encoder here emits one: profile 0, still
        // picture, reduced header, level 0, then a 16x16 frame size. Its frame
        // header states nothing, so the sequence header is the whole answer.
        let stream = temporal_unit(&[
            TEMPORAL_DELIMITER,
            &[0x0a, 0x04, 0x18, 0x0c, 0xff, 0xc0],
            &[0x1a, 0x01, 0x00],
        ]);
        let read = unit(&stream);
        assert!(read.key);
        assert_eq!(
            read.coded,
            Some(Coded {
                width: 16,
                height: 16
            })
        );
    }

    #[test]
    fn a_size_field_longer_than_one_byte_is_read_as_the_same_size() {
        // The libaom key frame with its size written over two LEB128 bytes.
        let stream = temporal_unit(&[
            TEMPORAL_DELIMITER,
            &[0x32, 0x84, 0x00, 0x10, 0x00, 0x8f, 0x00],
        ]);
        assert!(unit(&stream).key);
    }

    #[test]
    fn a_unit_that_is_not_an_obu_stream_holds_nothing() {
        // The forbidden bit is set on the first header an H.264 start code
        // would put there, so an Annex B unit read as this format stops at
        // its first byte.
        for stream in [
            &[0x80u8, 0x64, 0x00, 0x2a] as &[u8],
            &[],
            &[0x0a],
            &[0x12, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        ] {
            let read = unit(stream);
            assert!(!read.key);
            assert_eq!(read.coded, None);
        }
    }

    #[test]
    fn a_sequence_header_cut_off_partway_through_holds_no_size() {
        let whole = temporal_unit(&[TEMPORAL_DELIMITER, SEQ_VA_1920X1082]);
        for cut in 1..whole.len() - 1 {
            assert_eq!(unit(&whole[..cut]).coded, None);
        }
    }

    #[test]
    fn reading_stops_at_the_picture_and_does_not_walk_it() {
        // A sequence header behind the frame is never seen: the frame is the
        // end of what can change either answer.
        let stream = temporal_unit(&[
            TEMPORAL_DELIMITER,
            SEQ_VA_1920X1082,
            KEY_HEADER_VA,
            SEQ_AOM_2580X1080,
        ]);
        assert_eq!(
            unit(&stream).coded,
            Some(Coded {
                width: 1920,
                height: 1082
            })
        );
    }
}
