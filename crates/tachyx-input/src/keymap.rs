//! Building the scratch keymap that carries text.
//!
//! Characters arriving from a phone's on-screen keyboard have no key associated
//! with them. XKB will resolve a keysym written as `U` followed by a codepoint in
//! hexadecimal, so a keymap can be built that puts each needed character on a key
//! of its own, and pressing that key produces the character whatever layout the
//! host is using.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Keys available for characters.
///
/// Evdev keycodes run to 255, and entry `i` of the keymap becomes evdev keycode
/// `i + 1`, so this is the ceiling rather than a chosen limit.
const CAPACITY: usize = 255;

/// Characters that XKB will not accept in the `U`-prefixed form, because the
/// codepoint names a control character rather than something printable.
fn control_keysym(c: char) -> Option<&'static str> {
    match c {
        '\r' | '\n' => Some("Return"),
        '\t' => Some("Tab"),
        '\u{8}' => Some("BackSpace"),
        '\u{1b}' => Some("Escape"),
        '\u{7f}' => Some("Delete"),
        _ => None,
    }
}

/// The set of characters currently reachable, and the keymap that reaches them.
///
/// The set only grows. Uploading a keymap makes the compositor send a keymap
/// event to whichever application has focus, and some of them discard input
/// method state when that happens, so a keymap is rebuilt only when a character
/// arrives that no existing key can produce.
#[derive(Debug)]
pub(crate) struct TextKeymap {
    keys: BTreeMap<char, u32>,
}

impl TextKeymap {
    pub(crate) fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    /// The evdev keycode that produces `c`, if the current keymap has one.
    pub(crate) fn keycode(&self, c: char) -> Option<u32> {
        self.keys.get(&c).copied()
    }

    /// Adds every character of `text` that is not already reachable.
    ///
    /// Returns the new keymap when one is needed, and `None` when the existing
    /// keymap already covers the text. Characters beyond the keycode space are
    /// dropped, and the caller reports how many.
    pub(crate) fn admit(&mut self, text: &str) -> Option<String> {
        let before = self.keys.len();
        for c in text.chars() {
            if self.keys.len() >= CAPACITY {
                break;
            }
            let next = u32::try_from(self.keys.len()).expect("capacity bounds the count") + 1;
            self.keys.entry(c).or_insert(next);
        }
        (self.keys.len() != before).then(|| self.render())
    }

    fn render(&self) -> String {
        let mut by_code: Vec<(u32, char)> = self.keys.iter().map(|(c, code)| (*code, *c)).collect();
        by_code.sort_unstable();

        let mut xkb = String::from("xkb_keymap {\nxkb_keycodes \"tachyx\" {\nminimum = 8;\n");
        let maximum = by_code.last().map_or(9, |(code, _)| code + 8);
        writeln!(xkb, "maximum = {maximum};").expect("writing to a string cannot fail");
        for (code, _) in &by_code {
            writeln!(xkb, "<K{code}> = {};", code + 8).expect("writing to a string cannot fail");
        }
        xkb.push_str("};\nxkb_types \"tachyx\" { include \"complete\" };\n");
        xkb.push_str("xkb_compatibility \"tachyx\" { include \"complete\" };\n");
        // Naming the group is not decoration: without it the compositor fails to
        // read the keymap and silently keeps the previous one.
        xkb.push_str("xkb_symbols \"tachyx\" {\nname[Group1] = \"tachyx\";\n");
        for (code, c) in &by_code {
            let keysym =
                control_keysym(*c).map_or_else(|| format!("U{:04X}", *c as u32), ToOwned::to_owned);
            writeln!(xkb, "key <K{code}> {{ [ {keysym} ] }};")
                .expect("writing to a string cannot fail");
        }
        xkb.push_str("};\n};\n");
        xkb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_becomes_reachable_once_admitted() {
        let mut map = TextKeymap::new();
        assert_eq!(map.keycode('a'), None);
        assert!(map.admit("a").is_some());
        assert_eq!(map.keycode('a'), Some(1));
    }

    #[test]
    fn text_already_covered_does_not_rebuild_the_keymap() {
        let mut map = TextKeymap::new();
        map.admit("ab").expect("first admission builds one");
        assert!(
            map.admit("ba").is_none(),
            "rebuilding would disturb the focused application for nothing"
        );
    }

    #[test]
    fn characters_outside_the_basic_planes_survive() {
        let mut map = TextKeymap::new();
        let xkb = map
            .admit("\u{1F600}")
            .expect("a new character builds a keymap");
        assert!(xkb.contains("U1F600"), "{xkb}");
    }

    #[test]
    fn control_characters_use_their_names_rather_than_codepoints() {
        let mut map = TextKeymap::new();
        let xkb = map.admit("\n\t").expect("a new character builds a keymap");
        assert!(xkb.contains("Return"), "{xkb}");
        assert!(xkb.contains("Tab"), "{xkb}");
        assert!(
            !xkb.contains("U000A"),
            "a newline is not a printable codepoint"
        );
    }

    #[test]
    fn the_keymap_names_its_group() {
        let mut map = TextKeymap::new();
        let xkb = map.admit("x").expect("a new character builds a keymap");
        assert!(
            xkb.contains("name[Group1]"),
            "an unnamed group is rejected by the compositor"
        );
    }

    #[test]
    fn the_keycode_space_bounds_the_keymap() {
        let mut map = TextKeymap::new();
        let many: String = (0..400u32)
            .filter_map(|codepoint| char::from_u32(0x4E00 + codepoint))
            .collect();
        map.admit(&many);

        let admitted = many.chars().filter(|c| map.keycode(*c).is_some()).count();
        assert_eq!(admitted, CAPACITY, "every key up to the ceiling is used");

        let codes: Vec<u32> = many.chars().filter_map(|c| map.keycode(c)).collect();
        assert_eq!(*codes.iter().min().expect("keys were admitted"), 1);
        assert_eq!(
            u32::try_from(CAPACITY).expect("small"),
            *codes.iter().max().expect("keys were admitted"),
            "evdev keycodes stop at 255 and entry i becomes keycode i + 1"
        );
    }
}
