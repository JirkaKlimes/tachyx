//! Who is at each end of the handshake, and how each end decides.
//!
//! [`spec/pairing.md`](../../../spec/pairing.md) is the document. The short
//! version: a device is one Ed25519 key pair, the public half **is** the
//! identifier, and there is no certificate authority. **Both ends present a
//! certificate and both ends check one.** The host requires a client certificate
//! and refuses a device it has not paired with; the client accepts exactly the
//! host it was told to expect, and nothing else.
//!
//! Neither verifier looks at a validity date or a subject name. The certificate
//! is self-signed, so a forger writes whatever it likes into both, and the key is
//! the only thing an attacker has to break.
//!
//! There is no trust on first use here and there is no first-sight write. That
//! was the hole [`spec/protocol.md`](../../../spec/protocol.md) named, and
//! pairing is what closes it: a device this host has never paired with is
//! refused inside the handshake, before it can say anything at all.

use std::sync::Arc;

use quinn::Connection;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use tachyx_crypto::{DeviceId, PairedClients};

/// The name that goes in the TLS handshake, and that nothing reads.
///
/// A host on a local network has no name a public authority would sign, and
/// neither verifier examines one. It is here because rustls requires a server
/// name to be supplied, not because anything decides on it.
pub const SERVER_NAME: &str = "tachyx";

/// The cryptographic provider, named rather than defaulted.
///
/// quinn defaults to one provider and rustls to another, so taking both
/// defaults compiles both. Ring is the one named here: the other has an open
/// link failure on `aarch64-linux-android`, which is the client's main target.
/// Ring also carries Ed25519 for both signing and verification, which is the
/// only algorithm any tachyx device presents.
#[must_use]
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A certificate that is not one this build reads, or does not carry an Ed25519
/// key.
fn unreadable() -> TlsError {
    TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding)
}

/// A certificate that reads cleanly and names the wrong device.
fn wrong_device() -> TlsError {
    TlsError::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
}

/// The host's verifier: a client this host has paired with, and nobody else.
///
/// The list is read inside the handshake, so revoking a device takes effect on
/// the next connection rather than on a restart.
#[derive(Debug)]
pub struct Paired {
    clients: Arc<PairedClients>,
    provider: Arc<CryptoProvider>,
}

impl Paired {
    /// Accepts the devices in `clients`, using `provider` for the handshake
    /// signatures.
    #[must_use]
    pub const fn new(clients: Arc<PairedClients>, provider: Arc<CryptoProvider>) -> Self {
        Self { clients, provider }
    }
}

impl ClientCertVerifier for Paired {
    /// No hints. The client has exactly one certificate and no choice to make,
    /// and a hint list would name every paired device to anybody who asked.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let device = DeviceId::of_certificate(end_entity).map_err(|_| unreadable())?;
        if self.clients.get(device).is_some() {
            return Ok(ClientCertVerified::assertion());
        }
        // Loud on purpose. A paired phone that stops connecting is the fault an
        // operator will look for in this log, and a refusal that only appeared
        // at debug level would be an hour of looking in the wrong place.
        tracing::warn!(%device, "refused a device this host has not paired with");
        Err(wrong_device())
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

/// The dialling end's verifier for a peer the operator chose not to pin.
///
/// Any device with a well-formed Ed25519 certificate is accepted, and the
/// handshake signature still proves it holds the key it presents. What is given
/// up is knowing **which** device answered: anyone who can intercept the
/// address can be it. That is tolerable for exactly one peer, a relay written
/// without its key, because a session through a relay is end to end between two
/// pinned device keys whoever carries it; what an interceptor gains is the
/// metadata, who this device reaches and when, and the power to force the
/// relayed path. `spec/relay.md` is where that trade is weighed. Nothing else
/// may use this: a host is its key, and dialling one unpinned would be dialling
/// nobody in particular.
#[derive(Debug)]
pub struct AnyDevice {
    provider: Arc<CryptoProvider>,
}

impl AnyDevice {
    /// Accepts any device that proves it holds an Ed25519 key.
    #[must_use]
    pub const fn new(provider: Arc<CryptoProvider>) -> Self {
        Self { provider }
    }
}

impl ServerCertVerifier for AnyDevice {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let _ = DeviceId::of_certificate(end_entity).map_err(|_| unreadable())?;
        Ok(ServerCertVerified::assertion())
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

/// The dialling end's verifier: exactly one device, named before the connection
/// was made.
///
/// Used by a client for a host it has paired with, and by a peer for a relay
/// whose key travelled in its address. Both are the same decision: the expected
/// key, byte for byte, or nothing.
#[derive(Debug)]
pub struct Pinned {
    expected: DeviceId,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    /// Accepts `expected` and no other device.
    #[must_use]
    pub const fn new(expected: DeviceId, provider: Arc<CryptoProvider>) -> Self {
        Self { expected, provider }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let device = DeviceId::of_certificate(end_entity).map_err(|_| unreadable())?;
        if device == self.expected {
            return Ok(ServerCertVerified::assertion());
        }
        tracing::warn!(
            %device,
            expected = %self.expected,
            "the peer at that address is a different device"
        );
        Err(wrong_device())
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

/// Which device is at the far end of `connection`.
///
/// The certificate has already been through one of the verifiers above by the
/// time this can be called, so this reads a decision that has been made rather
/// than making one. It is what lets a session look up the peer's
/// [`Permissions`](tachyx_crypto::Permissions).
///
/// `None` only where no certificate was presented, which for these two
/// configurations means the handshake did not complete.
#[must_use]
pub fn peer_device(connection: &Connection) -> Option<DeviceId> {
    let chain = connection
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    DeviceId::of_certificate(chain.first()?).ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rustls::pki_types::UnixTime;
    use tachyx_crypto::store::PairedClient;
    use tachyx_crypto::{DeviceKey, Permissions};

    use super::*;

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::from_seed(&[seed; 32]).expect("a key")
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tachyx-verifier-{name}-{}/clients.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("a directory"));
        path
    }

    fn now() -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_700_000_000))
    }

    #[test]
    fn a_paired_device_is_accepted_and_a_stranger_is_not() {
        let paired = key(1);
        let stranger = key(2);
        let clients = PairedClients::open(scratch("paired")).expect("a list");
        clients
            .add(PairedClient {
                device: paired.id(),
                name: "phone".to_owned(),
                permissions: Permissions::default(),
                paired: 0,
            })
            .expect("the list is writable");

        let verifier = Paired::new(Arc::new(clients), provider());
        assert!(verifier
            .verify_client_cert(&paired.certificate().unwrap(), &[], now())
            .is_ok());
        assert!(
            verifier
                .verify_client_cert(&stranger.certificate().unwrap(), &[], now())
                .is_err(),
            "a device that has never paired must not reach the session at all"
        );
    }

    #[test]
    fn a_revoked_device_is_refused_by_the_next_handshake() {
        let device = key(3);
        let clients = Arc::new(PairedClients::open(scratch("revoke")).expect("a list"));
        clients
            .add(PairedClient {
                device: device.id(),
                name: "phone".to_owned(),
                permissions: Permissions::default(),
                paired: 0,
            })
            .expect("the list is writable");
        let verifier = Paired::new(Arc::clone(&clients), provider());
        let certificate = device.certificate().unwrap();
        assert!(verifier
            .verify_client_cert(&certificate, &[], now())
            .is_ok());

        assert!(clients.remove(device.id()).expect("the list is writable"));
        assert!(
            verifier
                .verify_client_cert(&certificate, &[], now())
                .is_err(),
            "the list is read inside the handshake, so revoking takes effect at once"
        );
    }

    #[test]
    fn the_pinned_verifier_takes_one_device_and_refuses_every_other() {
        let host = key(4);
        let impostor = key(5);
        let verifier = Pinned::new(host.id(), provider());
        let name = ServerName::try_from(SERVER_NAME).expect("a usable name");
        assert!(verifier
            .verify_server_cert(&host.certificate().unwrap(), &[], &name, &[], now())
            .is_ok());
        assert!(verifier
            .verify_server_cert(&impostor.certificate().unwrap(), &[], &name, &[], now())
            .is_err());
    }

    #[test]
    fn something_that_is_not_a_certificate_is_refused_rather_than_parsed() {
        let verifier = Pinned::new(key(6).id(), provider());
        let name = ServerName::try_from(SERVER_NAME).expect("a usable name");
        let rubbish = CertificateDer::from(vec![0u8; 32]);
        assert!(verifier
            .verify_server_cert(&rubbish, &[], &name, &[], now())
            .is_err());
    }
}
