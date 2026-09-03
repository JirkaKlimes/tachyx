//! The code on the host's screen.
//!
//! A host shows two codes for one pairing, and they are not the same secret. The
//! typed one is eight digits, which is what a person will retype. The scanned
//! one is sixteen bytes and travels in a QR with the host's own identifier
//! beside it.
//!
//! Two secrets rather than one because the strength of the two paths is
//! different and folding them into one would take the weaker. An attacker in the
//! middle of the mailbox that guesses the eight digits, one chance in a hundred
//! million and only once, is a true attacker in the middle: the host would pin the
//! attacker's key as the client's. The scanned path is 2⁻¹²⁸ against the same
//! attack, and the host's identifier in the QR closes it a second time. Giving
//! the scanned path the typed secret would throw all of that away for a shorter
//! `enum`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::device::{random, DeviceId};
use crate::{CryptoError, BASE32};

/// Digits in the typed secret.
const TYPED_DIGITS: u32 = 8;

/// One above the largest typed secret.
const TYPED_MODULUS: u32 = 10u32.pow(TYPED_DIGITS);

/// Bytes in the scanned secret.
const SCANNED_LEN: usize = 16;

/// What a pairing URI starts with, in the case it is written in.
///
/// Lower case, and it has to stay lower case: Android matches an intent filter's
/// scheme and host case-sensitively and does not normalise the incoming URI, so
/// an upper-case scheme reaches no application at all. The body after it is upper
/// case because that is what QR encodes in alphanumeric mode.
const URI_PREFIX: &str = "tachyx://pair?c=";

/// Which mailbox on the relay a pairing runs in.
///
/// Allocated by the relay, so two hosts cannot show the same one. Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Nameplate(std::num::NonZeroU16);

impl Nameplate {
    /// Names a mailbox, rejecting zero.
    #[must_use]
    pub const fn new(number: u16) -> Option<Self> {
        match std::num::NonZeroU16::new(number) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }

    /// The number the relay allocated.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl fmt::Display for Nameplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The secret half of a code, in whichever of the two forms the user used.
///
/// The two are separate variants and not one buffer, because the exchange feeds
/// them to the key derivation in written form and a session in which one end
/// believed it used one form and the other the second must not be possible to
/// write down.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PairingSecret {
    /// Read off the screen and typed.
    Typed(TypedSecret),
    /// Scanned out of the QR. A hundred and twenty-eight bits.
    Scanned([u8; SCANNED_LEN]),
}

/// Eight decimal digits, held below `100_000_000` by construction.
///
/// A newtype rather than a bare `u32`, because [`PairingCode::password`]
/// promises the typed form is always exactly eight characters after the colon,
/// and that is what keeps the two forms unable to collide. A value that broke
/// the promise could not collide either, but the invariant would then live in
/// a comment, and this crate's own rule is that it must not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TypedSecret(u32);

impl TypedSecret {
    /// Holds `digits`, refusing anything nine digits wide.
    #[must_use]
    pub const fn new(digits: u32) -> Option<Self> {
        if digits < TYPED_MODULUS {
            Some(Self(digits))
        } else {
            None
        }
    }

    /// The digits.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Never prints the digits, for the reason [`PairingSecret`]'s does not.
impl fmt::Debug for TypedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TypedSecret(..)")
    }
}

/// Never prints the secret. A pairing code in a log is a pairing code an
/// attacker can read at leisure.
impl fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Typed(_) => f.write_str("PairingSecret::Typed(..)"),
            Self::Scanned(_) => f.write_str("PairingSecret::Scanned(..)"),
        }
    }
}

/// Which of the two codes on the host's screen the client used.
///
/// It travels in the reply so the host knows which of its two exchanges to
/// finish. It is the peer's word and is not trusted for anything else: naming
/// the wrong one produces a record that will not open, which is the same outcome
/// as any other wrong code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodeForm {
    /// The eight digits.
    Typed,
    /// The QR.
    Scanned,
}

/// A mailbox and the secret that opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingCode {
    /// Which mailbox on the relay.
    pub nameplate: Nameplate,
    /// The secret, in the form the user gave it.
    pub secret: PairingSecret,
}

impl PairingCode {
    /// Which of the two forms this is.
    #[must_use]
    pub const fn form(&self) -> CodeForm {
        match self.secret {
            PairingSecret::Typed(_) => CodeForm::Typed,
            PairingSecret::Scanned(_) => CodeForm::Scanned,
        }
    }

    /// What the key exchange is keyed on.
    ///
    /// The nameplate is in it as well as the secret, so a code offered for one
    /// mailbox cannot be replayed into another. The two forms cannot collide:
    /// one is always eight characters after the colon and the other always
    /// twenty-six.
    pub(crate) fn password(&self) -> String {
        match self.secret {
            PairingSecret::Typed(digits) => {
                format!(
                    "{}:{:0width$}",
                    self.nameplate,
                    digits.get(),
                    width = TYPED_DIGITS as usize
                )
            }
            PairingSecret::Scanned(bytes) => {
                format!("{}:{}", self.nameplate, BASE32.encode(&bytes))
            }
        }
    }

    /// Reads a typed code, as `7-4921-8306`.
    ///
    /// Dashes and spaces after the nameplate are ignored, because somebody
    /// retyping eight digits will group them their own way.
    ///
    /// # Errors
    /// If there is no nameplate, or what follows is not eight digits. The error
    /// does not quote what was offered: a mistyped code is one character away
    /// from a live secret, and this would put it wherever the error is rendered.
    pub fn parse_typed(text: &str) -> Result<Self, CryptoError> {
        let fault = || CryptoError::NotACode;
        let (nameplate, rest) = text.trim().split_once('-').ok_or_else(fault)?;
        let nameplate = nameplate
            .parse()
            .ok()
            .and_then(Nameplate::new)
            .ok_or_else(fault)?;
        let digits: String = rest.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
        if digits.len() != TYPED_DIGITS as usize || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(fault());
        }
        let digits: u32 = digits.parse().map_err(|_| fault())?;
        Ok(Self {
            nameplate,
            secret: PairingSecret::Typed(TypedSecret::new(digits).ok_or_else(fault)?),
        })
    }
}

/// The pair of codes one pairing shows.
///
/// Both are live at once. The host runs an exchange for each and finishes
/// whichever the client answers.
///
/// Deliberately neither `Copy` nor `Clone`. `HostPairing::start` takes it by
/// value, so one draw can open at most one exchange, and *one attempt per code*
/// becomes a thing the compiler holds rather than a thing a caller remembers.
pub struct PairingCodes {
    /// The mailbox both codes name.
    pub nameplate: Nameplate,
    typed: TypedSecret,
    scanned: [u8; SCANNED_LEN],
}

/// Written by hand and says nothing but which mailbox. A derived one prints both
/// live secrets, and a pairing secret in a log is one an attacker reads at
/// leisure while the code is still on screen.
impl fmt::Debug for PairingCodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingCodes")
            .field("nameplate", &self.nameplate)
            .finish_non_exhaustive()
    }
}

impl PairingCodes {
    /// Draws a fresh pair of secrets for `nameplate`.
    ///
    /// # Panics
    /// If the system random source cannot be read, which is a machine that
    /// cannot do cryptography at all.
    #[must_use]
    pub fn fresh(nameplate: Nameplate) -> Self {
        // Rejection sampling rather than a remainder. 2^32 is not a multiple of
        // 10^8, so folding a uniform word into eight digits leaves the first
        // 94967296 values very slightly likelier. It costs nothing to not do
        // that: the loop retries about one time in fifty.
        let ceiling = TYPED_MODULUS * (u32::MAX / TYPED_MODULUS);
        let typed = loop {
            let drawn = u32::from_le_bytes(random::<4>());
            if let Some(held) = TypedSecret::new(drawn % TYPED_MODULUS)
                && drawn < ceiling
            {
                break held;
            }
        };
        Self {
            nameplate,
            typed,
            scanned: random::<SCANNED_LEN>(),
        }
    }

    /// The code for whichever form the client turns out to have used.
    #[must_use]
    pub const fn code(&self, form: CodeForm) -> PairingCode {
        PairingCode {
            nameplate: self.nameplate,
            secret: match form {
                CodeForm::Typed => PairingSecret::Typed(self.typed),
                CodeForm::Scanned => PairingSecret::Scanned(self.scanned),
            },
        }
    }

    /// The typed code, as it is shown: `7-4921-8306`.
    #[must_use]
    pub fn typed_text(&self) -> String {
        let digits = format!("{:0width$}", self.typed.get(), width = TYPED_DIGITS as usize);
        format!("{}-{}-{}", self.nameplate, &digits[..4], &digits[4..])
    }

    /// The QR's contents: this host, this relay, and the scanned secret.
    #[must_use]
    pub fn invite(&self, host: DeviceId, relay: String) -> PairingInvite {
        PairingInvite {
            nameplate: self.nameplate,
            secret: self.scanned,
            host,
            relay,
        }
    }
}

/// The version in front of a QR body, so a code a later version reshaped is
/// refused by name rather than decoded into something plausible.
const INVITE_VERSION: u8 = 0;

/// The longest pairing link this will read.
///
/// The longest one this build writes is 143 characters. A camera and an Android
/// deep link both hand this whatever they were given, so the bound is here for
/// the same reason `MAX_FLIGHT` is: decoding without one means allocating on a
/// stranger's word.
pub const MAX_URI: usize = 512;

/// What the QR carries.
///
/// The host's identifier is in here and is not in the typed code. That is the
/// whole difference between the two paths: a client that scanned compares the
/// identifier it was given against the one the host reveals, so an attacker who
/// somehow held the secret still cannot be the host.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingInvite {
    /// Which mailbox.
    pub nameplate: Nameplate,
    /// The scanned secret.
    pub secret: [u8; SCANNED_LEN],
    /// Whose desktop this is.
    pub host: DeviceId,
    /// Where the mailbox is, as `address:port`. The relay's own key is not here:
    /// the mailbox is protected by the exchange, not by the relay, and the
    /// authenticated relay address arrives inside it.
    pub relay: String,
}

/// The invite as it is encoded, which is a shape `postcard` writes tightly and
/// is not the shape the rest of the program reads.
#[derive(Serialize, Deserialize)]
struct Encoded {
    version: u8,
    host: DeviceId,
    secret: [u8; SCANNED_LEN],
    nameplate: Nameplate,
    relay: String,
}

/// Written by hand, for the reason [`PairingSecret`]'s is: this holds the live
/// scanned secret and a derived `Debug` would print it.
impl fmt::Debug for PairingInvite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingInvite")
            .field("nameplate", &self.nameplate)
            .field("host", &self.host)
            .field("relay", &self.relay)
            .finish_non_exhaustive()
    }
}

impl PairingInvite {
    /// The code this invite is, for the exchange.
    #[must_use]
    pub const fn code(&self) -> PairingCode {
        PairingCode {
            nameplate: self.nameplate,
            secret: PairingSecret::Scanned(self.secret),
        }
    }

    /// The URI the QR draws and a deep link carries.
    #[must_use]
    pub fn to_uri(&self) -> String {
        let body = postcard::to_stdvec(&Encoded {
            version: INVITE_VERSION,
            host: self.host,
            secret: self.secret,
            nameplate: self.nameplate,
            relay: self.relay.clone(),
        })
        .unwrap_or_default();
        format!("{URI_PREFIX}{}", BASE32.encode(&body))
    }

    /// Reads one back from [`PairingInvite::to_uri`].
    ///
    /// # Errors
    /// If the text is not a tachyx pairing URI, is longer than [`MAX_URI`], or
    /// carries a version this build does not read. The error quotes nothing: the
    /// scanned secret is inside the body.
    pub fn parse(uri: &str) -> Result<Self, CryptoError> {
        let fault = || CryptoError::NotACode;
        let uri = uri.trim();
        if uri.len() > MAX_URI {
            return Err(fault());
        }
        let body = uri
            .get(..URI_PREFIX.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(URI_PREFIX))
            .and_then(|_| uri.get(URI_PREFIX.len()..))
            .ok_or_else(fault)?;
        let bytes = BASE32.decode(body.as_bytes()).map_err(|_| fault())?;
        let encoded: Encoded = postcard::from_bytes(&bytes).map_err(|_| fault())?;
        if encoded.version != INVITE_VERSION {
            return Err(fault());
        }
        Ok(Self {
            nameplate: encoded.nameplate,
            secret: encoded.secret,
            host: encoded.host,
            relay: encoded.relay,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nameplate(number: u16) -> Nameplate {
        Nameplate::new(number).expect("a nameplate")
    }

    #[test]
    fn a_typed_code_survives_a_round_trip_through_what_is_shown() {
        let codes = PairingCodes::fresh(nameplate(7));
        let shown = codes.typed_text();
        assert_eq!(shown.len(), "7-4921-8306".len(), "one digit of nameplate");
        assert_eq!(
            PairingCode::parse_typed(&shown).unwrap(),
            codes.code(CodeForm::Typed)
        );
    }

    #[test]
    fn a_typed_code_is_read_however_the_person_grouped_it() {
        let codes = PairingCodes::fresh(nameplate(412));
        let wanted = codes.code(CodeForm::Typed);
        let shown = codes.typed_text();
        let bare = shown
            .split_once('-')
            .expect("a nameplate")
            .1
            .replace('-', "");
        for written in [
            format!("412-{bare}"),
            format!("412-{} {}", &bare[..2], &bare[2..]),
            format!("  412-{bare}  "),
        ] {
            assert_eq!(
                PairingCode::parse_typed(&written).unwrap(),
                wanted,
                "{written}"
            );
        }
    }

    #[test]
    fn text_that_is_not_a_code_is_refused() {
        for written in [
            "7-4921-830",   // seven digits
            "7-4921-83067", // nine
            "7-4921-830a",  // not a digit
            "0-4921-8306",  // no such mailbox
            "49218306",     // no nameplate
            "",
        ] {
            assert!(PairingCode::parse_typed(written).is_err(), "{written:?}");
        }
    }

    #[test]
    fn the_two_forms_never_key_the_same_exchange() {
        let codes = PairingCodes::fresh(nameplate(1));
        assert_ne!(
            codes.code(CodeForm::Typed).password(),
            codes.code(CodeForm::Scanned).password()
        );
    }

    #[test]
    fn a_code_is_bound_to_its_mailbox() {
        let here = PairingCodes::fresh(nameplate(1));
        let there = PairingCodes {
            nameplate: nameplate(2),
            ..here
        };
        assert_ne!(
            here.code(CodeForm::Typed).password(),
            there.code(CodeForm::Typed).password(),
            "the same digits in another mailbox is another exchange"
        );
    }

    #[test]
    fn an_invite_survives_a_round_trip_through_its_uri() {
        let codes = PairingCodes::fresh(nameplate(9));
        let host = crate::DeviceKey::from_seed(&[8u8; 32]).unwrap().id();
        let invite = codes.invite(host, "100.64.0.2:47990".to_owned());
        let uri = invite.to_uri();
        assert!(uri.starts_with("tachyx://pair?c="), "{uri}");
        assert_eq!(PairingInvite::parse(&uri).unwrap(), invite);
        assert_eq!(
            PairingInvite::parse(&uri).unwrap().code(),
            codes.code(CodeForm::Scanned)
        );
    }

    /// The QR must stay small enough to draw in a terminal, so this measures the
    /// code the terminal would actually draw rather than counting characters.
    ///
    /// A version 6 code is 41 modules, which with the four-module quiet zone and
    /// half-block glyphs is 49 columns by 25 rows. Wider than that stops fitting
    /// an eighty-column window, and the row count is already at the edge of a
    /// twenty-four row one.
    #[test]
    fn the_uri_draws_a_qr_a_terminal_can_show() {
        let host = crate::DeviceKey::from_seed(&[9u8; 32]).unwrap().id();
        for relay in ["100.64.0.2:47990", "relay.tachyx.example:47990"] {
            let uri = PairingCodes::fresh(nameplate(65535))
                .invite(host, relay.to_owned())
                .to_uri();
            let code =
                qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::L)
                    .expect("a pairing link always encodes");
            assert!(
                code.width() <= 41,
                "{relay}: {} characters is a {}-module code",
                uri.len(),
                code.width()
            );
        }
    }

    #[test]
    fn a_link_this_build_cannot_read_is_refused_rather_than_guessed_at() {
        for written in [
            "https://example.com/pair",
            "tachyx://pair?c=!!!!",
            "tachyx://pair?c=",
            "tachyx://session?c=AAAA",
        ] {
            assert!(PairingInvite::parse(written).is_err(), "{written:?}");
        }
    }

    #[test]
    fn the_scheme_is_read_in_any_case_because_a_scanner_may_shout_it() {
        let codes = PairingCodes::fresh(nameplate(3));
        let host = crate::DeviceKey::from_seed(&[10u8; 32]).unwrap().id();
        let invite = codes.invite(host, "10.0.0.1:47990".to_owned());
        let uri = invite.to_uri();
        let shouted = uri.replace("tachyx://pair?c=", "TACHYX://PAIR?C=");
        assert_eq!(PairingInvite::parse(&shouted).unwrap(), invite);
    }

    #[test]
    fn a_secret_is_never_printed() {
        let codes = PairingCodes::fresh(nameplate(5));
        let printed = format!("{:?}", codes.code(CodeForm::Typed).secret);
        assert!(!printed.contains(&codes.typed.get().to_string()), "{printed}");
    }

    #[test]
    fn the_typed_secret_is_always_eight_digits_wide() {
        for _ in 0..64 {
            let codes = PairingCodes::fresh(nameplate(1));
            assert!(codes.typed.get() < TYPED_MODULUS);
            let shown = codes.typed_text();
            assert_eq!(
                shown.split('-').skip(1).map(str::len).sum::<usize>(),
                TYPED_DIGITS as usize,
                "a leading zero must not shorten the code: {shown}"
            );
        }
    }
}
