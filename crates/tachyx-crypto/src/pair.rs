//! The exchange that turns a code into two devices that know each other.
//!
//! Three flights over a mailbox the relay cannot read. `CPace` over Ristretto255
//! turns the code into two keys, and each side then reveals what it knows sealed
//! under its direction's key. **A record that opens is the key confirmation**;
//! there are no separate confirmation MACs, because an AEAD that opens already
//! proves the peer derived the same key, and a second mechanism saying the same
//! thing would be a second thing to get wrong.
//!
//! The order the two sides reveal in is the security property worth stating
//! twice. The client reveals first and the host reveals only after the client's
//! record opened. So a wrong code, including an attacker in the middle of the
//! mailbox, never learns the host's identifier, its relay, or its addresses.
//!
//! Nothing here touches a socket. The three flights are byte strings, and
//! whatever carries them is somebody else's problem.

use std::net::SocketAddr;

use pake_cpace::CPace;
use serde::{Deserialize, Serialize};

use crate::code::{CodeForm, Nameplate, PairingCode, PairingCodes, PairingInvite};
use crate::device::{derive, DeviceId};
use crate::store::RelayAddress;
use crate::CryptoError;

/// The version in front of every flight.
///
/// `postcard` writes no field names, so a flight a later version reshaped would
/// decode into plausible nonsense rather than fail. Every one of the three
/// flights is checked against this before anything derived from it is used.
///
/// It is outside the sealed record, so it is the peer's word and it decides only
/// whether to keep reading. Nothing above it trusts it.
const FLIGHT_VERSION: u8 = 0;

/// What the AEAD binds each record to, so a record sealed as one flight cannot
/// be presented as another.
const HOST_AAD: &[u8] = b"tachyx pair host record v1";

/// The same, for the other direction.
const CLIENT_AAD: &[u8] = b"tachyx pair client record v1";

/// What one side is allowed to do once paired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    /// Whether input is injected, as against a picture only.
    pub control: bool,
    /// Whether a session opens without somebody at the host accepting it.
    ///
    /// There is no identifier-and-password unattended mode and there will not
    /// be one: that is the remote-support-scam vector. This is granted at
    /// pairing time by whoever ran the command, and being able to run it is the
    /// proof of presence.
    pub unattended: bool,
}

impl Default for Permissions {
    /// What the host grants unless told otherwise.
    ///
    /// Full control, because a viewer that cannot click is not what anybody is
    /// pairing a desktop for, and unattended, because a headless daemon has no
    /// screen to accept a session on. Both are narrowed from the command line.
    fn default() -> Self {
        Self {
            control: true,
            unattended: true,
        }
    }
}

/// What the client tells the host about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRecord {
    /// The client's identifier.
    pub device: DeviceId,
    /// What to call it in a list. Chosen by the peer, so it names a row and
    /// never decides anything.
    pub name: String,
}

/// What the host tells the client about itself, once the client's record has
/// opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    /// The host's identifier, which the client pins.
    pub device: DeviceId,
    /// What to call it in a list.
    pub name: String,
    /// Where to find this host when it is not on the same network. This is the
    /// authenticated copy: the relay named in the QR is only a hint about where
    /// the mailbox was.
    pub home_relay: Option<RelayAddress>,
    /// Addresses worth trying before the relay is asked.
    pub endpoints: Vec<SocketAddr>,
    /// What the host is granting.
    pub permissions: Permissions,
}

/// Flight one: the host's key exchange messages, one per code form.
///
/// Both are published because the host does not know which of the two codes on
/// its screen the client will use, and the two are different secrets on purpose.
/// See [`crate::code`].
#[derive(Debug, Serialize, Deserialize)]
struct Offer {
    version: u8,
    #[serde(with = "crate::fixed")]
    typed: [u8; pake_cpace::STEP1_PACKET_BYTES],
    #[serde(with = "crate::fixed")]
    scanned: [u8; pake_cpace::STEP1_PACKET_BYTES],
}

/// Flight two: which code the client used, its exchange message, and its record.
#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    version: u8,
    form: CodeForm,
    exchange: [u8; pake_cpace::STEP2_PACKET_BYTES],
    sealed: Vec<u8>,
}

/// Flight three: the host's record, sealed.
#[derive(Debug, Serialize, Deserialize)]
struct Grant {
    version: u8,
    sealed: Vec<u8>,
}

/// The host's side of a pairing, from the moment the codes are on screen.
///
/// [`HostPairing::finish`] takes `self` by value. That is the *one attempt per
/// code* rule, expressed so the compiler holds it: there is no second call to
/// make with a second guess.
pub struct HostPairing {
    codes: PairingCodes,
    typed: pake_cpace::Step1Out,
    scanned: pake_cpace::Step1Out,
}

/// Written by hand and says nothing. A derived one would print the exchange
/// state, and a pairing in a log is a pairing an attacker reads at leisure.
impl std::fmt::Debug for HostPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostPairing { .. }")
    }
}

impl HostPairing {
    /// Opens a pairing for `codes`, and produces flight one for the mailbox.
    ///
    /// # Errors
    /// If the key exchange will not start.
    pub fn start(codes: PairingCodes) -> Result<(Self, Vec<u8>), CryptoError> {
        let typed = step1(&codes.code(CodeForm::Typed))?;
        let scanned = step1(&codes.code(CodeForm::Scanned))?;
        let flight = encode(&Offer {
            version: FLIGHT_VERSION,
            typed: typed.packet(),
            scanned: scanned.packet(),
        })?;
        Ok((
            Self {
                codes,
                typed,
                scanned,
            },
            flight,
        ))
    }

    /// Which mailbox this pairing is running in.
    #[must_use]
    pub const fn nameplate(&self) -> Nameplate {
        self.codes.nameplate
    }

    /// The typed code, as it is shown: `7-4921-8306`.
    ///
    /// Read from here rather than from a [`PairingCodes`] the caller kept,
    /// because the caller does not keep one: `start` took it, and that is what
    /// makes a second exchange against these secrets impossible to write.
    #[must_use]
    pub fn typed_text(&self) -> String {
        self.codes.typed_text()
    }

    /// The QR's contents: this host, this relay, and the scanned secret.
    #[must_use]
    pub fn invite(&self, host: DeviceId, relay: String) -> PairingInvite {
        self.codes.invite(host, relay)
    }

    /// Reads the client's flight and produces the host's.
    ///
    /// # Errors
    /// [`CryptoError::WrongCode`] if the client did not have the code, which is
    /// the same variant a tampered flight produces and is deliberately not
    /// distinguished from it. Whatever the reason, the caller must burn the
    /// mailbox: this took the only attempt.
    pub fn finish(
        self,
        flight_two: &[u8],
        record: &HostRecord,
    ) -> Result<(ClientRecord, Vec<u8>, ShortAuth), CryptoError> {
        let reply: Reply = decode(flight_two)?;
        if reply.version != FLIGHT_VERSION {
            return Err(CryptoError::MalformedPairing);
        }
        let exchange = match reply.form {
            CodeForm::Typed => self.typed,
            CodeForm::Scanned => self.scanned,
        };
        let keys = exchange
            .step3(&reply.exchange)
            .map_err(|_| CryptoError::BadExchange)?;
        let client: ClientRecord = decode(&open(keys.k2, CLIENT_AAD, reply.sealed)?)?;
        let grant = encode(&Grant {
            version: FLIGHT_VERSION,
            sealed: seal(keys.k1, HOST_AAD, &encode(record)?),
        })?;
        Ok((client, grant, ShortAuth::of(&keys)))
    }
}

/// The client's side of a pairing.
pub struct ClientPairing {
    keys: pake_cpace::SharedKeys,
    /// The identifier the QR promised, when there was a QR. The host's record is
    /// refused if it names anything else.
    expected: Option<DeviceId>,
}

/// Written by hand because `SharedKeys` derives `Debug` and would print both
/// keys of a live pairing.
impl std::fmt::Debug for ClientPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientPairing")
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

impl ClientPairing {
    /// Reads the host's flight and produces the client's.
    ///
    /// `expected` is the host identifier a scanned code carried, and `None` for
    /// a typed one. When it is set, the host's record must name that device or
    /// the pairing fails: it is what makes the scanned path safe even if the
    /// exchange itself were broken.
    ///
    /// # Errors
    /// If the host's flight is not one this build reads, or the key exchange
    /// refuses it.
    pub fn answer(
        code: &PairingCode,
        flight_one: &[u8],
        expected: Option<DeviceId>,
        record: &ClientRecord,
    ) -> Result<(Self, Vec<u8>), CryptoError> {
        let offer: Offer = decode(flight_one)?;
        if offer.version != FLIGHT_VERSION {
            return Err(CryptoError::MalformedPairing);
        }
        let theirs = match code.form() {
            CodeForm::Typed => offer.typed,
            CodeForm::Scanned => offer.scanned,
        };
        let step2 = CPace::step2(
            &theirs,
            &code.password(),
            IDENTITY_HOST,
            IDENTITY_CLIENT,
            Some(ASSOCIATED_DATA),
        )
        .map_err(|_| CryptoError::BadExchange)?;
        let keys = step2.shared_keys();
        let flight = encode(&Reply {
            version: FLIGHT_VERSION,
            form: code.form(),
            exchange: step2.packet(),
            sealed: seal(keys.k2, CLIENT_AAD, &encode(record)?),
        })?;
        Ok((Self { keys, expected }, flight))
    }

    /// Reads the host's record.
    ///
    /// # Errors
    /// [`CryptoError::WrongCode`] if the record will not open, and
    /// [`CryptoError::WrongHost`] if it opens but names a device other than the
    /// one the scanned code promised.
    pub fn finish(self, flight_three: &[u8]) -> Result<(HostRecord, ShortAuth), CryptoError> {
        let grant: Grant = decode(flight_three)?;
        if grant.version != FLIGHT_VERSION {
            return Err(CryptoError::MalformedPairing);
        }
        let host: HostRecord = decode(&open(self.keys.k1, HOST_AAD, grant.sealed)?)?;
        if self.expected.is_some_and(|wanted| wanted != host.device) {
            return Err(CryptoError::WrongHost);
        }
        Ok((host, ShortAuth::of(&self.keys)))
    }
}

/// Three emoji both ends show, for somebody who wants to check the code was not
/// overheard.
///
/// Eighteen bits. It is a check a person makes, not one the protocol makes: the
/// exchange has already decided by the time this can be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortAuth([u8; 3]);

/// The words the three bytes name. Sixty-four of them, so each byte is one
/// symbol, chosen to be distinguishable at a glance and on a bad screen.
const WORDS: [&str; 64] = [
    "🐜", "🐝", "🐞", "🐢", "🐍", "🐙", "🐬", "🐟", "🐠", "🐡", "🦀", "🦋", "🐘", "🦒", "🦓", "🐴",
    "🐮", "🐷", "🐑", "🐐", "🐕", "🐈", "🐇", "🐁", "🦊", "🐻", "🐼", "🐨", "🦁", "🐯", "🐵", "🦉",
    "🐧", "🦆", "🦅", "🌵", "🌲", "🍁", "🌻", "🌹", "🍎", "🍋", "🍒", "🍇", "🍉", "🥕", "🌽", "🍄",
    "⭐", "🌙", "☁", "⚡", "🔥", "💧", "🌈", "❄", "⚓", "🔑", "🔔", "🎈", "🎁", "🎩", "🥁", "🎺",
];

impl ShortAuth {
    /// Derived from both keys, so neither end can steer it.
    fn of(keys: &pake_cpace::SharedKeys) -> Self {
        let mut material = [0u8; 64];
        material[..32].copy_from_slice(&keys.k1);
        material[32..].copy_from_slice(&keys.k2);
        let digest = derive(&material, b"tachyx pair sas v1");
        Self([digest[0], digest[1], digest[2]])
    }
}

impl std::fmt::Display for ShortAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c] = self.0.map(|byte| WORDS[usize::from(byte) % WORDS.len()]);
        write!(f, "{a} {b} {c}")
    }
}

/// The host's fixed name in the exchange.
///
/// A fixed string rather than a device identifier, because the typed code does
/// not know the host's identifier and both forms must build the same transcript.
const IDENTITY_HOST: &str = "tachyx-host";

/// The client's.
const IDENTITY_CLIENT: &str = "tachyx-client";

/// Bound into the generator point, so an exchange from another protocol using
/// this library cannot be replayed into this one.
const ASSOCIATED_DATA: &str = "tachyx-pair-v1";

/// The host's half of the exchange for one of its two codes.
fn step1(code: &PairingCode) -> Result<pake_cpace::Step1Out, CryptoError> {
    CPace::step1(
        &code.password(),
        IDENTITY_HOST,
        IDENTITY_CLIENT,
        Some(ASSOCIATED_DATA),
    )
    .map_err(|_| CryptoError::BadExchange)
}

/// Seals `plain` under `key`.
///
/// The nonce is zero and stays zero: each of the two keys the exchange produces
/// encrypts exactly one message, and the keys come from a fresh exchange every
/// time, so there is no second message for a nonce to repeat against. A counter
/// here would be a counter to get wrong.
fn seal(key: [u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, &key)
            .expect("a 32-byte ChaCha20-Poly1305 key"),
    );
    let mut sealed = plain.to_vec();
    key.seal_in_place_append_tag(
        ring::aead::Nonce::assume_unique_for_key([0u8; 12]),
        ring::aead::Aad::from(aad),
        &mut sealed,
    )
    .expect("sealing appends a tag and cannot overflow at these lengths");
    sealed
}

/// Opens what [`seal`] produced.
///
/// The failure is [`CryptoError::WrongCode`] whatever went wrong, because from
/// this side a wrong code, a tampered message and an attacker in the middle are
/// the same event and a finer answer would be a finer answer to the attacker.
fn open(key: [u8; 32], aad: &[u8], mut sealed: Vec<u8>) -> Result<Vec<u8>, CryptoError> {
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, &key)
            .expect("a 32-byte ChaCha20-Poly1305 key"),
    );
    let opened = key
        .open_in_place(
            ring::aead::Nonce::assume_unique_for_key([0u8; 12]),
            ring::aead::Aad::from(aad),
            &mut sealed,
        )
        .map_err(|_| CryptoError::WrongCode)?
        .len();
    sealed.truncate(opened);
    Ok(sealed)
}

/// The largest flight this will read.
///
/// A host record is a few hundred bytes. Anything near this is a peer that has
/// lost the framing, and reading it would mean allocating on a stranger's word.
pub const MAX_FLIGHT: usize = 8 * 1024;

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CryptoError> {
    postcard::to_stdvec(value).map_err(|_| CryptoError::MalformedPairing)
}

fn decode<T: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<T, CryptoError> {
    if bytes.len() > MAX_FLIGHT {
        return Err(CryptoError::MalformedPairing);
    }
    postcard::from_bytes(bytes).map_err(|_| CryptoError::MalformedPairing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::{PairingInvite, PairingSecret};
    use crate::device::DeviceKey;

    fn nameplate() -> Nameplate {
        Nameplate::new(7).expect("a nameplate")
    }

    fn codes() -> PairingCodes {
        PairingCodes::fresh(nameplate())
    }

    fn host_record(seed: u8) -> HostRecord {
        HostRecord {
            device: DeviceKey::from_seed(&[seed; 32]).unwrap().id(),
            name: "desk".to_owned(),
            home_relay: None,
            endpoints: vec!["10.0.0.4:47989".parse().unwrap()],
            permissions: Permissions::default(),
        }
    }

    fn client_record(seed: u8) -> ClientRecord {
        ClientRecord {
            device: DeviceKey::from_seed(&[seed; 32]).unwrap().id(),
            name: "phone".to_owned(),
        }
    }

    /// What a whole pairing left on both sides.
    #[derive(Debug)]
    struct Paired {
        /// What the host learned.
        client: ClientRecord,
        /// What the client learned.
        host: HostRecord,
        /// The words on the host's screen.
        shown_by_host: ShortAuth,
        /// The words on the client's.
        shown_by_client: ShortAuth,
    }

    /// Runs a whole pairing, reading the code the way a user would.
    ///
    /// `pick` is handed the live exchange and returns the code and the host
    /// identifier the user came away with, which is what separates the typed
    /// path from the scanned one.
    fn run(
        codes: PairingCodes,
        pick: impl FnOnce(&HostPairing) -> (PairingCode, Option<DeviceId>),
    ) -> Result<Paired, CryptoError> {
        let announced = host_record(1);
        let asking = client_record(2);
        let (opener, one) = HostPairing::start(codes)?;
        let (used, expected) = pick(&opener);
        let (joiner, two) = ClientPairing::answer(&used, &one, expected, &asking)?;
        let (client, three, shown_by_host) = opener.finish(&two, &announced)?;
        let (host, shown_by_client) = joiner.finish(&three)?;
        Ok(Paired {
            client,
            host,
            shown_by_host,
            shown_by_client,
        })
    }

    /// The typed path: the user reads the digits off the screen and retypes
    /// them, and learns nothing about the host until the host says so.
    fn typed(opener: &HostPairing) -> (PairingCode, Option<DeviceId>) {
        (
            PairingCode::parse_typed(&opener.typed_text()).expect("what is shown parses"),
            None,
        )
    }

    /// The scanned path: the QR carries the host's identifier as well.
    fn scanned(opener: &HostPairing) -> (PairingCode, Option<DeviceId>) {
        let host = host_record(1).device;
        let uri = opener.invite(host, "100.64.0.2:47990".to_owned()).to_uri();
        let read = PairingInvite::parse(&uri).expect("what is drawn parses");
        (read.code(), Some(read.host))
    }

    #[test]
    fn the_typed_code_pairs_two_devices() {
        let paired = run(codes(), typed).unwrap();
        assert_eq!(paired.client, client_record(2), "the host learned the client");
        assert_eq!(paired.host, host_record(1), "the client learned the host");
        assert_eq!(
            paired.shown_by_host, paired.shown_by_client,
            "both screens show the same words"
        );
    }

    #[test]
    fn the_scanned_code_pairs_two_devices() {
        let paired = run(codes(), scanned).unwrap();
        assert_eq!(paired.shown_by_host, paired.shown_by_client);
        assert_eq!(paired.host.device, host_record(1).device);
    }

    #[test]
    fn a_wrong_typed_code_learns_nothing_about_the_host() {
        let outcome = run(codes(), |opener| {
            let right = PairingCode::parse_typed(&opener.typed_text()).expect("parses");
            let PairingSecret::Typed(digits) = right.secret else {
                unreachable!("the typed form")
            };
            let wrong = crate::code::TypedSecret::new((digits.get() + 1) % 100_000_000)
                .expect("still eight digits");
            (
                PairingCode {
                    nameplate: right.nameplate,
                    secret: PairingSecret::Typed(wrong),
                },
                None,
            )
        });
        assert!(
            matches!(outcome, Err(CryptoError::WrongCode)),
            "a wrong guess must fail at the host, before the host has said anything: {outcome:?}"
        );
    }

    #[test]
    fn a_code_for_another_mailbox_does_not_open_this_one() {
        let outcome = run(codes(), |opener| {
            let right = PairingCode::parse_typed(&opener.typed_text()).expect("parses");
            (
                PairingCode {
                    nameplate: Nameplate::new(8).expect("a nameplate"),
                    secret: right.secret,
                },
                None,
            )
        });
        assert!(matches!(outcome, Err(CryptoError::WrongCode)), "{outcome:?}");
    }

    #[test]
    fn the_form_the_client_names_is_the_form_the_host_finishes() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, expected) = scanned(&opener);
        let (_, two) = ClientPairing::answer(&used, &one, expected, &client_record(2)).unwrap();
        // The right secret, announced as the form it is not. The host finishes
        // the other exchange, derives other keys, and the record will not open.
        let mut reply: Reply = decode(&two).unwrap();
        reply.form = CodeForm::Typed;
        assert!(matches!(
            opener.finish(&encode(&reply).unwrap(), &host_record(1)),
            Err(CryptoError::WrongCode)
        ));
    }

    #[test]
    fn a_tampered_client_record_is_refused() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, _) = typed(&opener);
        let (_, two) = ClientPairing::answer(&used, &one, None, &client_record(2)).unwrap();
        let mut reply: Reply = decode(&two).unwrap();
        let last = reply.sealed.len() - 1;
        reply.sealed[last] ^= 1;
        assert!(matches!(
            opener.finish(&encode(&reply).unwrap(), &host_record(1)),
            Err(CryptoError::WrongCode)
        ));
    }

    #[test]
    fn a_tampered_host_record_is_refused() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, _) = typed(&opener);
        let (joiner, two) = ClientPairing::answer(&used, &one, None, &client_record(2)).unwrap();
        let (_, three, _) = opener.finish(&two, &host_record(1)).unwrap();
        let mut grant: Grant = decode(&three).unwrap();
        grant.sealed[0] ^= 1;
        assert!(matches!(
            joiner.finish(&encode(&grant).unwrap()),
            Err(CryptoError::WrongCode)
        ));
    }

    #[test]
    fn a_scanned_pairing_refuses_a_host_that_is_not_the_one_in_the_qr() {
        let someone_else = DeviceKey::from_seed(&[99u8; 32]).unwrap().id();
        let outcome = run(codes(), |opener| {
            (scanned(opener).0, Some(someone_else))
        });
        assert!(
            matches!(outcome, Err(CryptoError::WrongHost)),
            "the identifier in the QR is the second lock on that path: {outcome:?}"
        );
    }

    #[test]
    fn two_pairings_of_one_nameplate_derive_different_keys() {
        let first = run(codes(), typed).unwrap();
        let second = run(codes(), typed).unwrap();
        assert_ne!(
            first.shown_by_host, second.shown_by_host,
            "each exchange draws fresh randomness, so the words differ every time"
        );
    }

    #[test]
    fn a_flight_this_build_cannot_read_is_refused_rather_than_guessed_at() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, _) = typed(&opener);
        assert!(matches!(
            ClientPairing::answer(&used, &vec![0u8; MAX_FLIGHT + 1], None, &client_record(2)),
            Err(CryptoError::MalformedPairing)
        ));
        assert!(matches!(
            opener.finish(b"not a flight", &host_record(1)),
            Err(CryptoError::MalformedPairing)
        ));
        let _ = one;
    }

    #[test]
    fn a_flight_from_another_version_is_refused_rather_than_misparsed() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, _) = typed(&opener);

        let mut ancient: Offer = decode(&one).unwrap();
        ancient.version = FLIGHT_VERSION.wrapping_add(1);
        assert!(
            matches!(
                ClientPairing::answer(&used, &encode(&ancient).unwrap(), None, &client_record(2)),
                Err(CryptoError::MalformedPairing)
            ),
            "postcard writes no field names, so an unread version is a silent misparse"
        );

        let (_, two) = ClientPairing::answer(&used, &one, None, &client_record(2)).unwrap();
        let mut reply: Reply = decode(&two).unwrap();
        reply.version = FLIGHT_VERSION.wrapping_add(1);
        assert!(matches!(
            opener.finish(&encode(&reply).unwrap(), &host_record(1)),
            Err(CryptoError::MalformedPairing)
        ));
    }

    #[test]
    fn a_grant_from_another_version_is_refused() {
        let (opener, one) = HostPairing::start(codes()).unwrap();
        let (used, _) = typed(&opener);
        let (joiner, two) = ClientPairing::answer(&used, &one, None, &client_record(2)).unwrap();
        let (_, three, _) = opener.finish(&two, &host_record(1)).unwrap();
        let mut grant: Grant = decode(&three).unwrap();
        grant.version = FLIGHT_VERSION.wrapping_add(1);
        assert!(matches!(
            joiner.finish(&encode(&grant).unwrap()),
            Err(CryptoError::MalformedPairing)
        ));
    }

    #[test]
    fn the_short_authentication_string_is_three_symbols() {
        let paired = run(codes(), typed).unwrap();
        assert_eq!(paired.shown_by_host.to_string().split(' ').count(), 3);
    }

    #[test]
    fn nothing_a_pairing_prints_carries_a_secret() {
        let (opener, _) = HostPairing::start(codes()).unwrap();
        let digits = opener
            .typed_text()
            .rsplit('-')
            .take(2)
            .collect::<Vec<_>>()
            .concat();
        let invite = opener.invite(host_record(1).device, "r:1".to_owned());
        for printed in [
            format!("{opener:?}"),
            format!("{invite:?}"),
            format!("{:?}", opener.codes),
        ] {
            assert!(!printed.contains(&digits), "{printed}");
        }
    }
}
