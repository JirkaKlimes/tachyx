//! Who the host is, and how a client comes to believe it.
//!
//! There is no certificate authority and there will not be one: a host on a
//! local network has no name a public authority would sign. What identifies a
//! host is its key, so a client remembers the key it saw the first time and
//! refuses anything else afterwards. That is trust on first use, and it is the
//! same verifier that pairing will use once pairing exists. Only the moment of
//! first acceptance changes: today it is automatic, later it is a code the user
//! confirms.

use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

/// The subject name the host puts in its certificate.
///
/// It is not checked by anything: the pin identifies the host, and a name that
/// resolves is exactly what a host on a local network does not have.
pub const SUBJECT: &str = "tachyx-host";

/// SHA-256 of a peer's subject public key info.
///
/// The public key rather than the whole certificate, so that a host which
/// regenerates its certificate around the same key keeps its identity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Takes the fingerprint of a certificate.
    ///
    /// # Errors
    /// If the bytes are not a certificate this build can parse.
    pub fn of(certificate: &CertificateDer<'_>) -> Result<Self, IdentityError> {
        let parsed = webpki::EndEntityCert::try_from(certificate)
            .map_err(|_| IdentityError::UnreadableCertificate)?;
        let spki = parsed.subject_public_key_info();
        let digest = ring::digest::digest(&ring::digest::SHA256, spki.as_ref());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        Ok(Self(bytes))
    }

    /// The fingerprint in full, as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        self.0
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// Reads a fingerprint back from [`Fingerprint::to_hex`].
    fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        let (pairs, _) = text.as_bytes().as_chunks::<2>();
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

/// The first eight bytes, grouped for someone to read aloud.
///
/// Sixty-four bits is short enough to compare across a room and long enough
/// that nobody will collide with it on a local network. Comparison always uses
/// the whole digest.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0[..8].iter().enumerate() {
            if index > 0 && index % 2 == 0 {
                write!(f, "-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// The host's own certificate and the key that signs it.
///
/// The key is persisted, so a restart does not look like a different host to
/// every client that has already pinned it.
#[derive(Debug)]
pub struct HostIdentity {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
}

impl HostIdentity {
    /// Loads the identity from `directory`, creating one on first run.
    ///
    /// # Errors
    /// If the directory cannot be read or written, or the stored key is not a
    /// key.
    pub fn load_or_create(directory: &Path) -> Result<Self, IdentityError> {
        fs::create_dir_all(directory)?;
        let key_path = directory.join("host.key.pem");
        let signing_key = match fs::read_to_string(&key_path) {
            Ok(pem) => rcgen::KeyPair::from_pem(&pem)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let generated = rcgen::KeyPair::generate()?;
                fs::write(&key_path, generated.serialize_pem())?;
                generated
            }
            Err(error) => return Err(error.into()),
        };
        // The certificate is derived rather than stored. It carries no
        // information the key does not, and deriving it means one file to keep
        // rather than two that can disagree.
        let certificate = rcgen::CertificateParams::new(vec![SUBJECT.to_owned()])?
            .self_signed(&signing_key)?
            .der()
            .clone();
        let fingerprint = Fingerprint::of(&certificate)?;
        Ok(Self {
            certificate,
            key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            fingerprint,
        })
    }

    /// What a client pins.
    #[must_use]
    pub const fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The certificate chain to present, which is one self-signed certificate.
    #[must_use]
    pub fn chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.certificate.clone()]
    }

    /// The key that signs the handshake.
    #[must_use]
    pub fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

/// The hosts a client has accepted, one line of `name fingerprint` each.
///
/// A file rather than a database, because a client that streams from more than
/// a handful of hosts does not exist.
#[derive(Debug)]
pub struct PinStore {
    path: PathBuf,
    pins: Mutex<Vec<(String, Fingerprint)>>,
}

impl PinStore {
    /// Opens the store at `path`, treating a missing file as an empty one.
    ///
    /// # Errors
    /// If the file exists and cannot be read.
    pub fn open(path: PathBuf) -> Result<Self, IdentityError> {
        let pins = match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter_map(|line| {
                    let (name, digest) = line.split_once(' ')?;
                    Some((name.to_owned(), Fingerprint::from_hex(digest)?))
                })
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            pins: Mutex::new(pins),
        })
    }

    /// What is pinned for `host`, if anything.
    #[must_use]
    pub fn get(&self, host: &str) -> Option<Fingerprint> {
        let pins = self
            .pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pins.iter()
            .find(|(name, _)| name == host)
            .map(|(_, print)| *print)
    }

    /// Records `fingerprint` as the identity of `host`, replacing any earlier
    /// one.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn set(&self, host: &str, fingerprint: Fingerprint) -> Result<(), IdentityError> {
        let text = {
            let mut pins = self
                .pins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pins.retain(|(name, _)| name != host);
            pins.push((host.to_owned(), fingerprint));
            pins.iter().fold(String::new(), |mut out, (name, print)| {
                let _ = writeln!(out, "{name} {}", print.to_hex());
                out
            })
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, text)?;
        Ok(())
    }
}

/// Accepts a host whose key matches the one already pinned for it, and pins the
/// key of a host that has none.
///
/// The certificate's own validity dates and subject name are not examined.
/// Neither says anything: the certificate is self-signed, so a forger writes
/// whatever they like into both, and the pin is what an attacker would have to
/// break.
#[derive(Debug)]
pub struct PinnedServer {
    pins: Arc<PinStore>,
    provider: Arc<CryptoProvider>,
}

impl PinnedServer {
    /// Verifies against `pins`, using `provider` for the handshake signatures.
    #[must_use]
    pub const fn new(pins: Arc<PinStore>, provider: Arc<CryptoProvider>) -> Self {
        Self { pins, provider }
    }
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let presented = Fingerprint::of(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        let host = pin_key(server_name);
        match self.pins.get(&host) {
            Some(pinned) if pinned == presented => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(TlsError::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
            None => {
                self.pins.set(&host, presented).map_err(|error| {
                    TlsError::General(format!("cannot record the host's identity: {error}"))
                })?;
                tracing::info!(%host, fingerprint = %presented, "pinned a new host");
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The name a pin is filed under.
///
/// A domain name is taken as written. Anything else is taken by its debug form,
/// which is stable for the addresses rustls can carry and is never confused
/// with a name, because a name cannot contain the brackets it prints.
fn pin_key(server_name: &ServerName<'_>) -> String {
    match server_name {
        ServerName::DnsName(name) => name.as_ref().to_owned(),
        other => format!("{other:?}"),
    }
}

/// Why an identity could not be established.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The key or pin file could not be read or written.
    #[error("cannot reach the identity on disk")]
    Storage(#[from] std::io::Error),
    /// The certificate machinery refused.
    #[error("cannot build the host's certificate")]
    Certificate(#[from] rcgen::Error),
    /// A certificate arrived that this build cannot parse.
    #[error("the peer presented something that is not a certificate")]
    UnreadableCertificate,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tachyx-identity-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn a_host_keeps_its_identity_across_a_restart() {
        let directory = scratch("restart");
        let first = HostIdentity::load_or_create(&directory).unwrap();
        let second = HostIdentity::load_or_create(&directory).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_fresh_key_is_a_different_host() {
        let one = HostIdentity::load_or_create(&scratch("one")).unwrap();
        let two = HostIdentity::load_or_create(&scratch("two")).unwrap();
        assert_ne!(one.fingerprint(), two.fingerprint());
    }

    #[test]
    fn a_pin_survives_a_round_trip_through_the_file() {
        let directory = scratch("pins");
        let identity = HostIdentity::load_or_create(&directory).unwrap();
        let path = directory.join("pins");
        let store = PinStore::open(path.clone()).unwrap();
        assert_eq!(store.get("desk"), None);
        store.set("desk", identity.fingerprint()).unwrap();

        let reopened = PinStore::open(path).unwrap();
        assert_eq!(reopened.get("desk"), Some(identity.fingerprint()));
        assert_eq!(reopened.get("laptop"), None);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_short_fingerprint_is_readable_and_stable() {
        let identity = HostIdentity::load_or_create(&scratch("display")).unwrap();
        let short = identity.fingerprint().to_string();
        assert_eq!(short.len(), 19, "eight bytes in four groups");
        assert!(identity
            .fingerprint()
            .to_hex()
            .starts_with(&short.replace('-', "")));
    }
}
