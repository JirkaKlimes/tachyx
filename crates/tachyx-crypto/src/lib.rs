//! Who a device is, and how two devices come to know each other.
//!
//! [`spec/pairing.md`](../../../spec/pairing.md) is the document this
//! implements, and it is the one to read first. The short version:
//!
//! A device is one Ed25519 key pair and the public key **is** the identifier.
//! There is no certificate authority, no account, and no server that can vouch
//! for a device. Access to a host is granted by pairing and by nothing else.
//!
//! Pairing runs a balanced password-authenticated key exchange over a mailbox on
//! a relay that cannot read it, and each side then reveals what it knows sealed
//! under a key only the code produces. A wrong code is one failed decryption and
//! the code is burnt.
//!
//! Nothing here does I/O except [`device::DeviceKey::load_or_create`] and
//! [`store`], and nothing here is async. Every decision is a pure function over
//! bytes, which is what makes it testable and what makes it worth auditing.

#![forbid(unsafe_code)]

pub mod code;
pub mod device;
pub mod pair;
pub mod store;

pub use code::{Nameplate, PairingCode, PairingInvite, PairingSecret};
pub use device::{DeviceId, DeviceKey, Signature};
pub use pair::{ClientPairing, ClientRecord, HostPairing, HostRecord, Permissions, ShortAuth};
pub use store::{KnownHosts, PairedClients, RelayAddress};

/// Why something in here refused.
///
/// Deliberately coarse on the cryptographic paths. "The code was wrong" and "the
/// message was tampered with" are the same variant because they are the same
/// event from the far side of an AEAD, and a finer report would be a finer
/// report to an attacker.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The key file could not be read or written.
    #[error("cannot reach the device key on disk")]
    Storage(#[from] std::io::Error),
    /// The stored seed is not a seed.
    #[error("the device key file does not hold a 32-byte seed")]
    MalformedKey,
    /// The certificate machinery refused.
    #[error("cannot build this device's certificate")]
    Certificate(#[from] rcgen::Error),
    /// A peer presented something that is not a certificate, or is not one this
    /// build accepts: the key in it must be Ed25519.
    #[error("the peer presented no usable Ed25519 certificate")]
    UnusableCertificate,
    /// A signature did not check out against the key that was supposed to have
    /// made it.
    #[error("the signature does not match the device it names")]
    BadSignature,
    /// Text that should have been a pairing code or a pairing link, and was
    /// not.
    ///
    /// It quotes nothing. A mistyped code is one character away from a live
    /// secret and a link carries one outright, so the shape is reported and the
    /// value is not.
    #[error("that is not a pairing code or a pairing link")]
    NotACode,
    /// Text that should have been a device identifier or a relay address, and
    /// was not.
    #[error("{what} is not written the way {kind} is written")]
    Unreadable {
        /// The kind of thing that was expected.
        kind: &'static str,
        /// What was offered instead, truncated.
        what: String,
    },
    /// The pairing peer's message would not decode.
    #[error("the pairing peer sent a message this build cannot read")]
    MalformedPairing,
    /// The pairing peer did not know the code. Also what a tampered message
    /// looks like, and deliberately not distinguished from it.
    #[error("the pairing peer did not have the code")]
    WrongCode,
    /// The key exchange itself refused the peer's group element.
    #[error("the pairing peer offered an unusable key exchange message")]
    BadExchange,
    /// The pairing succeeded and produced a host other than the one the scanned
    /// code named.
    ///
    /// Separate from [`CryptoError::BadSignature`] because nothing signed
    /// anything on that path: it is the identifier in the QR disagreeing with
    /// the identifier in the record, and it is the second lock the scanned path
    /// has and the typed one does not.
    #[error("the code named one host and another answered")]
    WrongHost,
    /// A record file could not be parsed.
    #[error("cannot read the paired devices in {path}")]
    Store {
        /// The file.
        path: std::path::PathBuf,
        /// Why.
        #[source]
        source: toml::de::Error,
    },
    /// A record file could not be produced, which is a fault in this build
    /// rather than in anything a peer sent.
    #[error("cannot write the paired devices in {path}: {detail}")]
    Encoding {
        /// The file.
        path: std::path::PathBuf,
        /// What the encoder said.
        detail: String,
    },
}

impl CryptoError {
    /// Reports unreadable text without quoting an unbounded amount of it back.
    pub(crate) fn unreadable(kind: &'static str, what: &str) -> Self {
        Self::Unreadable {
            kind,
            what: what.chars().take(64).collect(),
        }
    }
}

/// Base32 as everything a person reads or retypes is written.
///
/// Upper case out, because QR encodes `0-9 A-Z` in alphanumeric mode at 5.5 bits
/// a character where byte mode spends 8. Either case in, because it is a thing
/// people type.
pub(crate) const BASE32: data_encoding::Encoding = data_encoding::BASE32_NOPAD_NOCASE;

/// A byte array of a fixed length, written as those bytes and nothing else.
///
/// `postcard` encodes an array this way already, but serde derives the traits
/// only up to 32 elements, and a signature is 64 and a key exchange message is
/// 48. Writing it as a tuple keeps the encoding identical to the derived one,
/// which is what stops the length from also being on the wire.
pub(crate) mod fixed {
    use std::fmt;

    use serde::de::{self, SeqAccess, Visitor};
    use serde::ser::SerializeTuple as _;
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(N)?;
        for byte in bytes {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[u8; N], D::Error> {
        struct ExactlyN<const N: usize>;

        impl<'de, const N: usize> Visitor<'de> for ExactlyN<N> {
            type Value = [u8; N];

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{N} bytes")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
                let mut bytes = [0u8; N];
                for (index, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(index, &self))?;
                }
                Ok(bytes)
            }
        }

        deserializer.deserialize_tuple(N, ExactlyN::<N>)
    }
}
