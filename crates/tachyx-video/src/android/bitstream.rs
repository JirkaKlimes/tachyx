//! What the client has to read out of the bytes it is handed.
//!
//! The host repeats the parameter sets in the stream before every key frame, so
//! the decoder configures itself and needs no separate configuration step, no
//! step to repeat after a reconnection, and none after a resolution change.
//! Two things still have to be read here, and both are read from the stream
//! rather than from the host's stream configuration message, because the stream
//! is what the decoder is actually going to see:
//!
//! - the coded size, which `MediaCodec` demands before it will start, and which
//!   changes mid-stream when the host changes resolution;
//! - whether a unit can be decoded on its own, which is where a decoder that has
//!   fallen behind is allowed to start again.
//!
//! Everything here is arithmetic over a byte slice, so all of it is exercised by
//! the tests at the bottom against sequence parameter sets taken off this host's
//! own encoder.

/// A coded slice of a picture that refers back to others.
const NAL_SLICE: u8 = 1;

/// A coded slice of an instantaneous decoding refresh picture.
const NAL_IDR_SLICE: u8 = 5;

/// A sequence parameter set.
const NAL_SPS: u8 = 7;

/// Profiles whose sequence parameter set carries the chroma format and the
/// scaling lists before the picture size.
const PROFILES_WITH_CHROMA: [u32; 13] =
    [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// The picture size a sequence parameter set declares, in whole macroblocks.
///
/// This is what the decoder writes and what the surface it writes into has to be
/// sized for. It is not what the viewer should see: H.264 states a size the
/// macroblock grid cannot hold, such as 1080 rows or 2580 columns, by coding the
/// next multiple up and attaching a crop rectangle. Nothing on the Android
/// present path applies that rectangle, so the visible size travels in the
/// host's stream configuration message and the application applies it when it
/// draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Coded {
    /// Width in pixels, always a multiple of sixteen.
    pub(super) width: u16,
    /// Height in pixels, always a multiple of sixteen.
    pub(super) height: u16,
}

impl Coded {
    /// Reads the coded size out of a sequence parameter set.
    ///
    /// `sps` is the unit including its one-byte header and still carrying the
    /// emulation prevention bytes the byte stream format inserts.
    ///
    /// Everything before the size has to be read to reach it. Everything after
    /// it is the application's business rather than the decoder's, so parsing
    /// stops at `frame_mbs_only_flag` and never touches the crop rectangle.
    pub(super) fn of(sps: &[u8]) -> Option<Self> {
        let mut bits = Bits::after_header(sps)?;
        let profile = bits.bits(8)?;
        bits.bits(8)?;
        bits.bits(8)?;
        bits.unsigned()?;

        if PROFILES_WITH_CHROMA.contains(&profile) {
            let chroma = bits.unsigned()?;
            if chroma == 3 {
                bits.bit()?;
            }
            bits.unsigned()?;
            bits.unsigned()?;
            bits.bit()?;
            if bits.bit()? == 1 {
                bits.skip_scaling_lists(if chroma == 3 { 12 } else { 8 })?;
            }
        }

        bits.unsigned()?;
        match bits.unsigned()? {
            0 => {
                bits.unsigned()?;
            }
            1 => {
                bits.bit()?;
                bits.signed()?;
                bits.signed()?;
                let cycle = bits.unsigned()?;
                // The specification caps this at 255. A larger one is a
                // misparse or a corrupt unit, and reading it would be a loop
                // over a number a hostile stream chose.
                if cycle > 255 {
                    return None;
                }
                for _ in 0..cycle {
                    bits.signed()?;
                }
            }
            _ => {}
        }
        bits.unsigned()?;
        bits.bit()?;

        let width_in_macroblocks = bits.unsigned()?.checked_add(1)?;
        let height_in_map_units = bits.unsigned()?.checked_add(1)?;
        let fields = 2 - bits.bit()?;

        Some(Self {
            width: u16::try_from(width_in_macroblocks.checked_mul(16)?).ok()?,
            height: u16::try_from(height_in_map_units.checked_mul(16 * fields)?).ok()?,
        })
    }
}

/// What one access unit turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AccessUnit<'a> {
    /// The sequence parameter set it carries, if it carries one.
    pub(super) parameters: Option<&'a [u8]>,
    /// Whether it can be decoded with nothing before it.
    pub(super) key: bool,
}

impl<'a> AccessUnit<'a> {
    /// Reads what an Annex B access unit contains.
    ///
    /// Only the two facts above are wanted, so this walks the start codes and
    /// looks at one byte of each unit. It never copies and never allocates, and
    /// it stops at the first coded slice: the parameter sets come before the
    /// picture and the picture is the end of the unit, so nothing past there can
    /// change either answer. That is what keeps a scan for start codes off the
    /// two hundred kilobytes of a key frame.
    pub(super) fn of(bytes: &'a [u8]) -> Self {
        let mut unit = Self {
            parameters: None,
            key: false,
        };
        for nal in Nals::of(bytes) {
            match nal.first().map(|header| header & 0x1f) {
                Some(NAL_SPS) => unit.parameters = Some(nal),
                Some(NAL_IDR_SLICE) => {
                    unit.key = true;
                    break;
                }
                Some(NAL_SLICE) => break,
                _ => {}
            }
        }
        unit
    }
}

/// The units of an Annex B byte stream.
///
/// A unit runs to the next start code, so the zero byte of a four-byte start
/// code is left on the end of the unit before it. Nothing here reads that far:
/// a unit is only ever asked for its header byte or parsed as a parameter set,
/// and a parameter set ends long before its trailing bytes.
struct Nals<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Nals<'a> {
    fn of(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let start = next_start_code(self.bytes, self.at)? + 3;
        let end = next_start_code(self.bytes, start).unwrap_or(self.bytes.len());
        self.at = end;
        self.bytes.get(start..end)
    }
}

/// Where the next three-byte start code prefix begins, at or after `from`.
fn next_start_code(bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|offset| from + offset)
}

/// A reader over a unit's payload, in the order the syntax is written.
///
/// It takes the emulation prevention bytes back out as it goes rather than
/// rewriting the unit first, which keeps the whole path free of an allocation
/// per key frame.
struct Bits<'a> {
    bytes: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    /// Reads a unit from just after its one-byte header.
    fn after_header(nal: &'a [u8]) -> Option<Self> {
        if nal.is_empty() {
            return None;
        }
        Some(Self {
            bytes: nal,
            byte: 1,
            bit: 0,
        })
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.bytes.get(self.byte)?;
        let value = u32::from(byte >> (7 - self.bit)) & 1;
        if self.bit == 7 {
            self.bit = 0;
            self.byte += 1;
            // `00 00 03` in the byte stream is `00 00` in the syntax: the third
            // byte is only there to stop the pair looking like a start code.
            if self.bytes.get(self.byte) == Some(&3)
                && self.bytes[self.byte - 2..self.byte] == [0, 0]
            {
                self.byte += 1;
            }
        } else {
            self.bit += 1;
        }
        Some(value)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    /// An unsigned Exp-Golomb code.
    fn unsigned(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            // No syntax element in a sequence parameter set is this wide, so
            // past here the unit is not one and the loop is a hostile stream's
            // to choose the length of.
            if leading > 31 {
                return None;
            }
        }
        Some((1u32 << leading) - 1 + self.bits(leading)?)
    }

    /// A signed Exp-Golomb code.
    fn signed(&mut self) -> Option<i32> {
        let code = self.unsigned()?;
        let magnitude = i64::from(code.div_ceil(2));
        i32::try_from(if code % 2 == 0 { -magnitude } else { magnitude }).ok()
    }

    /// Steps over the scaling lists, which stand between the profile and the
    /// picture size and are never used here.
    fn skip_scaling_lists(&mut self, lists: usize) -> Option<()> {
        for list in 0..lists {
            if self.bit()? == 0 {
                continue;
            }
            let mut last = 8;
            let mut next = 8;
            for _ in 0..if list < 6 { 16 } else { 64 } {
                if next != 0 {
                    next = (last + self.signed()? + 256) % 256;
                }
                if next != 0 {
                    last = next;
                }
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessUnit, Coded};

    /// A 3440x1440 sequence parameter set, taken off this host's own encoder.
    const SPS_3440X1440: &[u8] = &[
        0x27, 0x64, 0x00, 0x32, 0xac, 0x11, 0x12, 0x80, 0x35, 0xc0, 0xb5, 0xbf, 0xf0, 0x00, 0x10,
        0x00, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x9f, 0x08, 0x84,
        0x6e,
    ];

    /// The one the same session changed to: 2580x1080 visible, so 2592x1088
    /// coded, with the difference expressed as a crop nothing here reads.
    const SPS_2592X1088: &[u8] = &[
        0x27, 0x64, 0x00, 0x32, 0xac, 0x11, 0x12, 0x80, 0x28, 0x80, 0x89, 0xe7, 0x97, 0xff, 0x00,
        0x01, 0x00, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc9, 0xf0,
        0x88, 0x46, 0xe0,
    ];

    /// And the one it changed to next, a rung down the ladder.
    const SPS_1728X720: &[u8] = &[
        0x27, 0x64, 0x00, 0x20, 0xac, 0x11, 0x12, 0x80, 0x6c, 0x05, 0xbe, 0x5f, 0xff, 0x00, 0x01,
        0x00, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc9, 0xf0, 0x88,
        0x46, 0xe0,
    ];

    /// 1920x1080 asked for, 1088 coded. From the same encoder at a rung the
    /// ladder does reach, and the reason every rung is a multiple of sixteen.
    const SPS_1920X1088: &[u8] = &[
        0x27, 0x64, 0x00, 0x2a, 0xac, 0x72, 0x20, 0x1e, 0x00, 0x89, 0xf9, 0x7f, 0xf0, 0x00, 0x10,
        0x00, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x78, 0x9f, 0x08, 0x84,
        0x6e,
    ];

    /// A picture parameter set, which carries no size and must not be read as
    /// though it did.
    const PPS: &[u8] = &[0x28, 0xee, 0x3c, 0x30];

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for nal in nals {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(nal);
        }
        stream
    }

    #[test]
    fn a_parameter_set_states_the_size_the_decoder_writes() {
        assert_eq!(
            Coded::of(SPS_3440X1440),
            Some(Coded {
                width: 3440,
                height: 1440
            })
        );
        assert_eq!(
            Coded::of(SPS_1920X1088),
            Some(Coded {
                width: 1920,
                height: 1088
            })
        );
    }

    #[test]
    fn a_size_the_macroblock_grid_cannot_hold_is_read_as_the_size_that_is_coded() {
        // 2580x1080 and 1710x720 on the wire; the decoder writes the rounded up
        // sizes below and the crop that says so is never applied on this path.
        assert_eq!(
            Coded::of(SPS_2592X1088),
            Some(Coded {
                width: 2592,
                height: 1088
            })
        );
        assert_eq!(
            Coded::of(SPS_1728X720),
            Some(Coded {
                width: 1728,
                height: 720
            })
        );
    }

    #[test]
    fn the_size_changes_when_the_host_changes_resolution() {
        let sizes: Vec<Coded> = [SPS_3440X1440, SPS_2592X1088, SPS_1728X720]
            .into_iter()
            .filter_map(Coded::of)
            .collect();
        assert_eq!(sizes.len(), 3);
        assert_ne!(sizes[0], sizes[1]);
        assert_ne!(sizes[1], sizes[2]);
    }

    #[test]
    fn nothing_that_is_not_a_parameter_set_is_read_as_one() {
        assert_eq!(Coded::of(PPS), None);
        assert_eq!(Coded::of(&[]), None);
        assert_eq!(Coded::of(&[0x27]), None);
        // A parameter set cut off partway through, which is what a lost
        // fragment would leave behind.
        for cut in 1..SPS_3440X1440.len() - 4 {
            let _ = Coded::of(&SPS_3440X1440[..cut]);
        }
    }

    #[test]
    fn a_key_frame_is_the_parameter_sets_and_a_refresh_slice() {
        let stream = annex_b(&[&[0x09, 0x10], SPS_3440X1440, PPS, &[0x25, 0x88, 0x80]]);
        let unit = AccessUnit::of(&stream);
        assert!(unit.key);
        assert_eq!(
            unit.parameters.and_then(Coded::of),
            Coded::of(SPS_3440X1440)
        );
    }

    #[test]
    fn a_later_frame_carries_neither() {
        let stream = annex_b(&[&[0x09, 0x30], &[0x21, 0x9a, 0x08]]);
        let unit = AccessUnit::of(&stream);
        assert!(!unit.key);
        assert_eq!(unit.parameters, None);
    }

    #[test]
    fn three_byte_start_codes_are_read_as_well_as_four() {
        let mut stream = vec![0, 0, 1];
        stream.extend_from_slice(SPS_1920X1088);
        stream.extend_from_slice(&[0, 0, 1, 0x25, 0x88]);
        let unit = AccessUnit::of(&stream);
        assert!(unit.key);
        assert_eq!(
            unit.parameters.and_then(Coded::of),
            Coded::of(SPS_1920X1088)
        );
    }

    #[test]
    fn reading_stops_at_the_picture_and_does_not_walk_it() {
        // A slice several hundred kilobytes long is the ordinary case, and
        // walking it for start codes would be the whole cost of reading a unit.
        // Whatever follows one cannot change either answer, so a second
        // parameter set behind the slice is never seen.
        let mut stream = annex_b(&[&[0x09, 0x10], SPS_1920X1088, PPS, &[0x25, 0x88, 0x80]]);
        stream.extend_from_slice(&annex_b(&[SPS_3440X1440]));
        let unit = AccessUnit::of(&stream);
        assert!(unit.key);
        assert_eq!(
            unit.parameters.and_then(Coded::of),
            Coded::of(SPS_1920X1088)
        );
    }

    #[test]
    fn a_unit_with_no_start_code_at_all_holds_nothing() {
        let unit = AccessUnit::of(&[0x27, 0x64, 0x00, 0x2a]);
        assert!(!unit.key);
        assert_eq!(unit.parameters, None);
    }

    #[test]
    fn the_bytes_that_stop_a_start_code_appearing_are_taken_back_out() {
        // The 3440x1440 set carries `00 00 03` three times. Reading it as
        // written would run the picture size off into the escape bytes.
        assert!(SPS_3440X1440.windows(3).any(|window| window == [0, 0, 3]));
        assert_eq!(
            Coded::of(SPS_3440X1440).map(|coded| coded.width),
            Some(3440)
        );
    }
}
