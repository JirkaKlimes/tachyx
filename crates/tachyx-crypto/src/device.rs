//! What a device is: one Ed25519 key pair, whose public half is its name.
//!
//! There is no certificate authority and there will not be one. A host on a
//! local network has no name a public authority would sign, and an account that
//! could grant access to a desktop would be a thing worth stealing. So the key
//! is the identity, both ends present one, and both ends compare the exact key
//! they were paired with.

use std::fmt;
use std::fs;
use std::path::Path;

use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize as _, Zeroizing};

use crate::{CryptoError, BASE32};

/// Bytes in the seed on disk, in the derived signing seed, and in an identifier.
const SEED_LEN: usize = 32;

/// Bytes in an Ed25519 signature.
const SIGNATURE_LEN: usize = 64;

/// The name of the file holding the seed, inside the state directory.
const KEY_FILE: &str = "device.key";

/// PKCS#8 v1 in front of a raw Ed25519 seed: a sequence, version zero, the
/// `id-Ed25519` algorithm identifier, and an octet string wrapping an octet
/// string of 32 bytes.
///
/// Written out rather than reached through an encoder because it never varies:
/// the only thing after it is the seed.
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Subject public key info in front of a raw Ed25519 public key: a sequence, the
/// `id-Ed25519` algorithm identifier, and a bit string of 33 bytes whose first
/// is the count of unused bits.
///
/// A peer's certificate must carry exactly this, which is how a certificate
/// holding a key of some other kind is refused before anything reads it.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The name that goes in the certificate, and that nothing checks.
///
/// It is not examined by either end: the certificate is self-signed, so a forger
/// writes what it likes into the name and the dates, and the key is the only
/// thing an attacker has to break.
const SUBJECT: &str = "tachyx";

/// A device: the public half of an Ed25519 key pair, and nothing else.
///
/// Comparison is over all 32 bytes. The short form [`fmt::Display`] writes is
/// for a person reading a list and is never what a decision is made on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId([u8; SEED_LEN]);

impl DeviceId {
    /// Takes an identifier from bytes that are already known to be a public key.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SEED_LEN]) -> Self {
        Self(bytes)
    }

    /// The identifier as the 32 bytes it is.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SEED_LEN] {
        &self.0
    }

    /// The identifier in full, as 52 base32 characters.
    #[must_use]
    pub fn to_base32(self) -> String {
        BASE32.encode(&self.0)
    }

    /// Reads one back from [`DeviceId::to_base32`], in either case.
    ///
    /// # Errors
    /// If the text is not 52 base32 characters.
    pub fn parse(text: &str) -> Result<Self, CryptoError> {
        let bytes = BASE32
            .decode(text.trim().as_bytes())
            .map_err(|_| CryptoError::unreadable("a device identifier", text))?;
        let bytes: [u8; SEED_LEN] = bytes
            .try_into()
            .map_err(|_| CryptoError::unreadable("a device identifier", text))?;
        Ok(Self(bytes))
    }

    /// Reads the identifier out of a peer's certificate.
    ///
    /// The certificate's dates and subject are not examined, and the key must be
    /// Ed25519: a certificate carrying any other kind of key is refused here
    /// rather than compared and found different.
    ///
    /// # Errors
    /// If the bytes are not a certificate this build can parse, or the key in it
    /// is not Ed25519.
    pub fn of_certificate(certificate: &CertificateDer<'_>) -> Result<Self, CryptoError> {
        let parsed = webpki::EndEntityCert::try_from(certificate)
            .map_err(|_| CryptoError::UnusableCertificate)?;
        let spki = parsed.subject_public_key_info();
        let spki = spki.as_ref();
        let (prefix, key) = spki
            .split_at_checked(SPKI_PREFIX.len())
            .ok_or(CryptoError::UnusableCertificate)?;
        if prefix != SPKI_PREFIX {
            return Err(CryptoError::UnusableCertificate);
        }
        let key: [u8; SEED_LEN] = key
            .try_into()
            .map_err(|_| CryptoError::UnusableCertificate)?;
        Ok(Self(key))
    }

    /// Checks `signature` over `message` in `domain`.
    ///
    /// `domain` separates one thing this key signs from another, so a signature
    /// collected in one place cannot be presented in a second. See
    /// [`DeviceKey::sign`].
    ///
    /// # Errors
    /// If the signature was not made by this device over exactly these bytes.
    pub fn verify(
        self,
        domain: &[u8],
        message: &[u8],
        signature: &Signature,
    ) -> Result<(), CryptoError> {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, self.0)
            .verify(&bind(domain, message), &signature.0)
            .map_err(|_| CryptoError::BadSignature)
    }
}

/// The first twelve base32 characters, in groups of four.
///
/// Sixty bits: short enough to read across a room, long enough that nothing on
/// one person's list collides. Every comparison uses all 52.
impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let full = self.to_base32();
        let mut groups = full.as_bytes().chunks(4).take(3);
        if let Some(first) = groups.next() {
            write!(f, "{}", String::from_utf8_lossy(first))?;
        }
        for group in groups {
            write!(f, "-{}", String::from_utf8_lossy(group))?;
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; SIGNATURE_LEN]);

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signature(..)")
    }
}

/// Written as 64 bare bytes rather than as a length-prefixed byte string, which
/// is what `postcard` does with a fixed-size array and what serde declines to
/// derive above 32.
impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(SIGNATURE_LEN)?;
        for byte in &self.0 {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Exactly64;

        impl<'de> Visitor<'de> for Exactly64 {
            type Value = Signature;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("64 bytes of Ed25519 signature")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Signature, A::Error> {
                let mut bytes = [0u8; SIGNATURE_LEN];
                for (index, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(index, &"64 bytes"))?;
                }
                Ok(Signature(bytes))
            }
        }

        deserializer.deserialize_tuple(SIGNATURE_LEN, Exactly64)
    }
}

/// This device's own key: the secret, and everything derived from it.
///
/// The certificate is derived rather than stored, because it carries nothing the
/// key does not and two files that can disagree are worse than one.
pub struct DeviceKey {
    pair: ring::signature::Ed25519KeyPair,
    pkcs8: Zeroizing<Vec<u8>>,
    id: DeviceId,
}

/// Written by hand and says only which device this is.
///
/// A derived one prints `pkcs8`, whose bytes 16 to 48 are the signing seed. One
/// `tracing::debug!(?key)` or one panic payload would then publish the device
/// identity, and `ring`'s own redacted `Debug` would have been defeated by the
/// derive in front of it.
impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl DeviceKey {
    /// Loads the key from `directory`, creating one on first run.
    ///
    /// The file is 32 bytes of seed at mode 0600. A headless daemon cannot reach
    /// a keyring. There is no session bus, no unlocked collection and no user
    /// to unlock one, so this is a file, protected the way an SSH host key is
    /// protected. Saying so is better than a keyring that is not there.
    ///
    /// # Errors
    /// If the directory cannot be read or written, or the file is not a seed.
    pub fn load_or_create(directory: &Path) -> Result<Self, CryptoError> {
        fs::create_dir_all(directory)?;
        let path = directory.join(KEY_FILE);
        let mut seed = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let fresh = random::<SEED_LEN>();
                write_private(&path, &fresh)?;
                fresh.to_vec()
            }
            Err(error) => return Err(error.into()),
        };
        let result = match <[u8; SEED_LEN]>::try_from(seed.as_slice()) {
            Ok(bytes) => Self::from_seed(&bytes),
            Err(_) => Err(CryptoError::MalformedKey),
        };
        seed.zeroize();
        result
    }

    /// Builds a key from a seed already in hand, which is what a test and the
    /// relay's `--key` both want.
    ///
    /// # Errors
    /// If the certificate machinery refuses the key.
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Result<Self, CryptoError> {
        let mut signing = derive(seed, b"tachyx device ed25519 v1");
        // The version 1 form, which carries the seed and not the public key.
        // ring's own `from_pkcs8` wants version 2 and refuses this; rustls and
        // rcgen both read it through `from_pkcs8_maybe_unchecked`, so the key
        // this process signs with is built from the seed directly and these
        // bytes exist only to hand to those two.
        //
        // It is the signing seed in the clear for the life of this object. That
        // is what rustls needs to be handed, so it cannot be avoided; the
        // wrapper clears it on drop, which is the part that can be.
        let mut pkcs8 = Zeroizing::new(Vec::with_capacity(PKCS8_PREFIX.len() + SEED_LEN));
        pkcs8.extend_from_slice(&PKCS8_PREFIX);
        pkcs8.extend_from_slice(&signing);
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&signing)
            .map_err(|_| CryptoError::MalformedKey)?;
        signing.zeroize();
        let id = DeviceId(
            <[u8; SEED_LEN]>::try_from(ring::signature::KeyPair::public_key(&pair).as_ref())
                .map_err(|_| CryptoError::MalformedKey)?,
        );
        Ok(Self { pair, pkcs8, id })
    }

    /// This device's identifier, which is the public half of this key.
    #[must_use]
    pub const fn id(&self) -> DeviceId {
        self.id
    }

    /// The certificate to present, derived from the key at every call.
    ///
    /// # Errors
    /// If the certificate machinery refuses.
    pub fn certificate(&self) -> Result<CertificateDer<'static>, CryptoError> {
        let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(self.pkcs8.as_slice()),
            &rcgen::PKCS_ED25519,
        )?;
        Ok(rcgen::CertificateParams::new(vec![SUBJECT.to_owned()])?
            .self_signed(&key)?
            .der()
            .clone())
    }

    /// The private key rustls signs the handshake with.
    ///
    /// Every call hands out a fresh copy of the signing seed on the heap, and
    /// that copy is the caller's to drop. Ask once, at the point the TLS
    /// configuration is built.
    #[must_use]
    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.pkcs8.to_vec()).into()
    }

    /// Signs `message` in `domain`.
    ///
    /// `domain` is a fixed string naming what is being signed, and it is part of
    /// what the signature covers. Without it a signature made over one structure
    /// could be presented as a signature over a second structure that happens to
    /// start with the same bytes.
    ///
    /// # Panics
    /// If Ed25519 produces something other than 64 bytes, which it does not.
    #[must_use]
    pub fn sign(&self, domain: &[u8], message: &[u8]) -> Signature {
        let signature = self.pair.sign(&bind(domain, message));
        Signature(
            <[u8; SIGNATURE_LEN]>::try_from(signature.as_ref())
                .expect("Ed25519 signatures are 64 bytes"),
        )
    }
}

/// What is actually signed: the domain, its length, then the message.
///
/// The length goes in so that no two `(domain, message)` pairs can produce the
/// same bytes. Domains are fixed constants and none is longer than 255 bytes.
fn bind(domain: &[u8], message: &[u8]) -> Vec<u8> {
    let length = u8::try_from(domain.len()).expect("a signing domain is a short constant");
    let mut bound = Vec::with_capacity(1 + domain.len() + message.len());
    bound.push(length);
    bound.extend_from_slice(domain);
    bound.extend_from_slice(message);
    bound
}

/// One 32-byte value derived from `seed` under `info`.
///
/// The seed on disk is never used as a key directly. It costs one hash and it
/// means a second kind of key can be added later without a second file and
/// without a migration.
pub(crate) fn derive(seed: &[u8], info: &[u8]) -> [u8; 32] {
    /// The output length, as ring's HKDF wants it expressed.
    struct Bytes32;

    impl ring::hkdf::KeyType for Bytes32 {
        fn len(&self) -> usize {
            32
        }
    }

    let mut out = [0u8; 32];
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"")
        .extract(seed)
        .expand(&[info], Bytes32)
        .and_then(|okm| okm.fill(&mut out))
        .expect("HKDF-SHA256 produces 32 bytes for any input");
    out
}

/// `N` bytes from the system's random source.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system random source is readable");
    bytes
}

/// Writes `bytes` to `path`, readable by nobody else.
///
/// The mode is set as the file is created rather than after, so there is no
/// moment at which the seed exists and is world readable.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("tachyx-device-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn a_device_keeps_its_identity_across_a_restart() {
        let directory = scratch("restart");
        let first = DeviceKey::load_or_create(&directory).unwrap();
        let second = DeviceKey::load_or_create(&directory).unwrap();
        assert_eq!(first.id(), second.id());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_fresh_seed_is_a_different_device() {
        let one = DeviceKey::load_or_create(&scratch("one")).unwrap();
        let two = DeviceKey::load_or_create(&scratch("two")).unwrap();
        assert_ne!(one.id(), two.id());
    }

    #[test]
    fn the_certificate_carries_the_identifier_and_nothing_else_identifies_it() {
        let key = DeviceKey::from_seed(&[7u8; 32]).unwrap();
        let first = DeviceId::of_certificate(&key.certificate().unwrap()).unwrap();
        let second = DeviceId::of_certificate(&key.certificate().unwrap()).unwrap();
        assert_eq!(
            first,
            key.id(),
            "the certificate names the key that made it"
        );
        assert_eq!(
            first, second,
            "a certificate derived twice from one key names it twice"
        );
    }

    #[test]
    fn a_signature_is_bound_to_its_domain() {
        let key = DeviceKey::from_seed(&[1u8; 32]).unwrap();
        let signature = key.sign(b"tachyx-offer-v1", b"payload");
        key.id()
            .verify(b"tachyx-offer-v1", b"payload", &signature)
            .expect("its own domain checks out");
        assert!(
            key.id()
                .verify(b"tachyx-answer-v1", b"payload", &signature)
                .is_err(),
            "a signature collected in one place must not be presentable in another"
        );
    }

    #[test]
    fn a_signature_from_another_device_is_refused() {
        let mine = DeviceKey::from_seed(&[1u8; 32]).unwrap();
        let theirs = DeviceKey::from_seed(&[2u8; 32]).unwrap();
        let signature = theirs.sign(b"d", b"payload");
        assert!(mine.id().verify(b"d", b"payload", &signature).is_err());
    }

    #[test]
    fn an_identifier_survives_a_round_trip_through_text_in_either_case() {
        let id = DeviceKey::from_seed(&[3u8; 32]).unwrap().id();
        let text = id.to_base32();
        assert_eq!(text.len(), 52, "32 bytes is 52 base32 characters");
        assert_eq!(DeviceId::parse(&text).unwrap(), id);
        assert_eq!(DeviceId::parse(&text.to_lowercase()).unwrap(), id);
        assert!(DeviceId::parse("not an identifier").is_err());
        assert!(
            DeviceId::parse(&text[..51]).is_err(),
            "a truncated identifier is not a shorter identifier"
        );
    }

    #[test]
    fn the_short_form_is_a_prefix_of_the_long_one() {
        let id = DeviceKey::from_seed(&[4u8; 32]).unwrap().id();
        let short = id.to_string();
        assert_eq!(short.len(), 14, "twelve characters in three groups");
        assert!(id.to_base32().starts_with(&short.replace('-', "")));
    }

    #[test]
    fn a_signature_survives_a_round_trip_through_postcard_as_64_bare_bytes() {
        let key = DeviceKey::from_seed(&[5u8; 32]).unwrap();
        let signature = key.sign(b"d", b"m");
        let bytes = postcard::to_stdvec(&signature).unwrap();
        assert_eq!(bytes.len(), 64, "no length prefix in front of a fixed size");
        assert_eq!(
            postcard::from_bytes::<Signature>(&bytes).unwrap(),
            signature
        );
    }

    #[test]
    fn an_identifier_costs_32_bytes_on_the_wire() {
        let id = DeviceKey::from_seed(&[6u8; 32]).unwrap().id();
        assert_eq!(postcard::to_stdvec(&id).unwrap().len(), 32);
    }

    #[cfg(unix)]
    #[test]
    fn the_seed_is_written_readable_by_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = scratch("mode");
        DeviceKey::load_or_create(&directory).unwrap();
        let mode = fs::metadata(directory.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&directory).unwrap();
    }
}
