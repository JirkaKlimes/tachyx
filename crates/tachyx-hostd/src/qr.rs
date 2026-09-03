//! The pairing link, drawn where somebody can point a camera at it.

use std::fmt::Write as _;

use qrcode::render::unicode::Dense1x2;
use qrcode::types::QrError;
use qrcode::{EcLevel, QrCode};

/// Black ink. Set at the start of every row.
///
/// The basic sixteen rather than a 24-bit colour: every terminal has these, and
/// even a theme that reads them loosely leaves a contrast a scanner is happy
/// with.
const INK: &str = "\x1b[30;47m";

/// Back to whatever the terminal was doing, at the end of every row.
const PLAIN: &str = "\x1b[0m";

/// The error correction a code drawn on a clean screen needs.
///
/// The lowest level, and a screen is the case it was meant for: there is no
/// print, no fold and no coffee. It is also what keeps the link inside the 41
/// modules a terminal can show. See `spec/pairing.md`.
const CORRECTION: EcLevel = EcLevel::L;

/// What the terminal a code is being drawn on can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    /// Whether the locale says it can show half-block glyphs. When it cannot,
    /// the code is drawn one `#` a module, which is half as wide but squeezed
    /// vertically.
    pub unicode: bool,
    /// Whether to paint the code black on white.
    ///
    /// Right for a terminal and wrong for a pipe, which wants the characters
    /// and not the escape sequences around them.
    pub painted: bool,
}

impl Terminal {
    /// What the terminal this process is writing to can do.
    #[must_use]
    pub fn probe() -> Self {
        use std::io::IsTerminal as _;

        Self {
            unicode: utf8_locale(),
            painted: std::io::stdout().is_terminal(),
        }
    }
}

/// Draws `uri` as a QR code, quiet zone and all.
///
/// **The polarity is not left to the terminal.** `Dense1x2` paints the dark
/// modules and leaves the light ones as spaces, so the code comes out the right
/// way round on a light terminal and inverted on a dark one, and a scanner
/// refuses an inverted code. So the ink is stated: black on white, every row,
/// whatever theme the terminal is in.
///
/// # Errors
/// If the link is too long to encode, which for a pairing link means a relay
/// with a very long name.
pub fn draw(uri: &str, terminal: Terminal) -> Result<String, QrError> {
    let code = QrCode::with_error_correction_level(uri, CORRECTION)?;
    let drawn = if terminal.unicode {
        code.render::<Dense1x2>().quiet_zone(true).build()
    } else {
        let square = code
            .render::<char>()
            .quiet_zone(true)
            .dark_color('#')
            .light_color(' ')
            .build();
        // Two cells a module. A terminal cell is about twice as tall as it is
        // wide, so one cell a module draws a code half as wide as it is high,
        // and a decoder that measures its modules refuses that.
        square
            .lines()
            .map(|row| {
                row.chars()
                    .flat_map(|cell| [cell, cell])
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !terminal.painted {
        return Ok(drawn + "\n");
    }
    let mut painted = String::with_capacity(drawn.len() + drawn.lines().count() * 10);
    for row in drawn.lines() {
        let _ = writeln!(painted, "{INK}{row}{PLAIN}");
    }
    Ok(painted)
}

/// Whether the locale this process was started in is a UTF-8 one.
///
/// A locale that says nothing is taken as UTF-8, because that is what every
/// terminal that says nothing has been for years. A locale that names something
/// else is believed.
fn utf8_locale() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_none_or(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{check_address, ADDRESS_LIMIT};
    use tachyx_crypto::code::PairingCodes;
    use tachyx_crypto::{DeviceKey, Nameplate};

    /// The longest link a pairing can produce: the highest mailbox number, and a
    /// relay address of exactly the length `relay add` accepts.
    ///
    /// Taken from the bound rather than from a name somebody chose, because a
    /// literal here would measure one relay and say nothing about the next one.
    fn longest_link() -> String {
        let host = DeviceKey::from_seed(&[9u8; 32]).expect("a key").id();
        let address = format!("{}:47990", "r".repeat(ADDRESS_LIMIT - ":47990".len()));
        check_address(&address).expect("the longest address `relay add` takes");
        PairingCodes::fresh(Nameplate::new(u16::MAX).expect("a nameplate"))
            .invite(host, address)
            .to_uri()
    }

    #[test]
    fn the_longest_pairing_link_stays_inside_the_41_modules_a_terminal_can_show() {
        let uri = longest_link();
        let code = QrCode::with_error_correction_level(&uri, CORRECTION).expect("a link encodes");
        assert!(
            code.width() <= 41,
            "an address of {ADDRESS_LIMIT} characters is a {} character link and a {}-module \
             code: either the bound comes down or the terminal cannot draw this",
            uri.len(),
            code.width()
        );
    }

    #[test]
    fn the_longest_pairing_link_is_49_columns_by_25_rows_of_half_blocks() {
        let drawn = draw(
            &longest_link(),
            Terminal {
                unicode: true,
                painted: false,
            },
        )
        .expect("a link draws");
        let rows: Vec<&str> = drawn.lines().collect();
        assert_eq!(rows.len(), 25, "two modules of quiet zone and 41 of code");
        for row in rows {
            assert_eq!(row.chars().count(), 49, "{row}");
        }
    }

    #[test]
    fn a_painted_code_states_its_own_colours_on_every_row() {
        let drawn = draw(
            "tachyx://pair?c=AAAA",
            Terminal {
                unicode: true,
                painted: true,
            },
        )
        .expect("a link draws");
        for row in drawn.lines() {
            assert!(
                row.starts_with(INK) && row.ends_with(PLAIN),
                "a row that inherits the terminal's colours is inverted on a dark theme: {row:?}"
            );
        }
    }

    #[test]
    fn a_terminal_that_cannot_show_a_block_glyph_gets_one_that_is_only_ascii() {
        let uri = "tachyx://pair?c=AAAA";
        let drawn = draw(
            uri,
            Terminal {
                unicode: false,
                painted: false,
            },
        )
        .expect("a link draws");
        assert!(drawn.is_ascii(), "{drawn}");
        assert!(drawn.contains('#'));

        // Twice as wide as it is high, in cells, which is square on a terminal.
        // One cell a module is a code stretched 1:2, and a decoder that
        // measures its modules refuses that.
        let modules = QrCode::with_error_correction_level(uri, CORRECTION)
            .expect("a link encodes")
            .width()
            + 8;
        let rows: Vec<&str> = drawn.lines().collect();
        assert_eq!(rows.len(), modules, "four modules of quiet zone each side");
        for row in rows {
            assert_eq!(row.chars().count(), modules * 2, "{row}");
        }
    }
}
