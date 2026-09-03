//! The tachyx relay.
//!
//! [`spec/relay.md`](../../../spec/relay.md) is the document. The relay does
//! four jobs and holds no session key while doing any of them: presence,
//! rendezvous, telling each peer the address it was seen from, and forwarding
//! opaque datagrams between two peers whose hole punch did not work.
//!
//! A hostile relay can drop traffic, delay it, lie about a reflexive address,
//! and watch who talks to whom and how much. It cannot read a session, join one,
//! or inject into one, because the session's own TLS runs end to end through it.
//! That is what makes a community relay and a paid fleet interchangeable, and it
//! is the property every decision here keeps.
//!
//! Nothing is stored. A relay that is restarted has forgotten everything, and
//! every host reconnects.

#![forbid(unsafe_code)]

pub mod limits;
pub mod session;
pub mod splice;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::{Endpoint, EndpointConfig, TokioRuntime};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use tachyx_crypto::{DeviceId, DeviceKey, RelayAddress};
use tachyx_relay::proto::ALPN;

use crate::limits::Limits;
use crate::session::{Serving, serve};
use crate::splice::{Lanes, SpliceSocket, canonical};
use crate::state::Registry;

/// How many connection attempts may be part way through the handshake at once.
///
/// quinn defaults to 65536, which at a typical Ethernet packet is under 100 MiB
/// of memory a stranger can make this process hold. A relay accepts a connection
/// in microseconds, so a queue this deep is already more slack than it needs.
const MAX_INCOMING: usize = 256;

/// Bytes buffered for all connection attempts that have not been accepted.
///
/// quinn defaults to 100 MiB. Nothing sends 0-RTT data to a relay, so the only
/// thing this holds is handshake packets.
const INCOMING_BUFFER: u64 = 4 * 1024 * 1024;

/// How long a control connection may be silent before it is dropped.
///
/// The peer keeps it alive every twenty seconds, so three missed keepalives.
const IDLE_MS: u32 = 60_000;

/// How often stale mailboxes are swept and the rate limiter's own map is
/// cleared.
const SWEEP: Duration = Duration::from_secs(30);

/// Why the relay could not start, or could not serve one peer.
#[derive(Debug, thiserror::Error)]
pub enum RelaydError {
    /// The socket could not be opened or bound.
    #[error("the socket could not be used")]
    Socket(#[from] std::io::Error),
    /// The socket, or the framing on the control stream, would not work.
    #[error(transparent)]
    Transport(#[from] tachyx_transport::TransportError),
    /// The device key could not be read, or would not make a certificate.
    #[error(transparent)]
    Identity(#[from] tachyx_crypto::CryptoError),
    /// The TLS configuration was refused.
    #[error("the TLS configuration was refused")]
    Tls(#[from] rustls::Error),
    /// This build cannot negotiate TLS 1.3, which QUIC requires.
    #[error("this build cannot negotiate TLS 1.3, which QUIC requires")]
    NoTls13,
    /// The connection ended.
    #[error("the connection ended")]
    Connection(#[from] quinn::ConnectionError),
    /// The control stream stopped carrying messages.
    #[error(transparent)]
    Control(#[from] tachyx_transport::ControlError),
    /// A peer said something this build does not read.
    #[error(transparent)]
    Protocol(#[from] tachyx_relay::RelayError),
    /// Bytes that are not a message at all.
    #[error("a peer sent a message this build cannot read")]
    Malformed,
    /// The handshake completed with no certificate, so there is no name to
    /// route by. This build's verifier does not allow it.
    #[error("a peer presented no certificate to be named by")]
    Anonymous,
    /// The connection went away while it was being set up.
    #[error("the connection went away")]
    Gone,
    /// The endpoint has been closed.
    #[error("the relay is closed")]
    Closed,
}

/// Accepts any well-formed self-signed Ed25519 certificate, and takes its key as
/// the peer's identity.
///
/// **The relay authorises nothing.** It needs a stable name to route by, not a
/// decision about who may talk to whom. That decision belongs to the host, which
/// makes it in its own TLS handshake against its own paired list.
///
/// What is still checked is that the certificate parses and carries an Ed25519
/// key, because a certificate that does not is a certificate with no
/// [`DeviceId`] in it and therefore nothing to route by.
#[derive(Debug)]
struct AnyDevice {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for AnyDevice {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        DeviceId::of_certificate(end_entity)
            .map(|_| ClientCertVerified::assertion())
            .map_err(|_| TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
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
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
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

/// The configuration a relay listens with.
///
/// # Errors
/// If the key will not make a certificate, or this build cannot negotiate TLS
/// 1.3.
fn server_config(key: &DeviceKey) -> Result<quinn::ServerConfig, RelaydError> {
    let provider = tachyx_transport::provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyDevice { provider }))
        .with_single_cert(vec![key.certificate()?], key.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|_| RelaydError::NoTls13)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));

    let mut limits = quinn::TransportConfig::default();
    limits
        .max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
            IDLE_MS,
        ))))
        // A relay is reached across the internet, where a packet of a local
        // network's size is black-holed until discovery notices.
        .initial_mtu(tachyx_transport::RELAYED_MTU)
        // One stream carries control. Spliced traffic is not on this connection
        // at all: it is beside it, on the same port.
        .max_concurrent_bidi_streams(quinn::VarInt::from_u32(1))
        .max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    config.transport_config(Arc::new(limits));
    config
        .max_incoming(MAX_INCOMING)
        .incoming_buffer_size_total(INCOMING_BUFFER);
    Ok(config)
}

/// A relay, bound and ready to serve.
#[derive(Debug)]
pub struct Relay {
    endpoint: Endpoint,
    device: DeviceId,
    registry: Arc<Registry>,
    lanes: Arc<Lanes>,
    limits: Arc<Limits>,
}

impl Relay {
    /// Binds a relay at `listen`, presenting `key`.
    ///
    /// # Errors
    /// If the socket cannot be opened, or the key will not make a TLS
    /// configuration.
    pub fn bind(listen: SocketAddr, key: &DeviceKey) -> Result<Self, RelaydError> {
        let lanes = Arc::new(Lanes::new());
        // The same socket hygiene a session's socket gets: four MiB of buffer
        // asked for, and the expedited forwarding mark, because what this
        // forwards is somebody's video.
        let socket = tachyx_transport::bind_socket(listen)?;
        let socket = SpliceSocket::new(socket, Arc::clone(&lanes))?;
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config(key)?),
            socket,
            Arc::new(TokioRuntime),
        )?;
        Ok(Self {
            endpoint,
            device: key.id(),
            registry: Arc::new(Registry::default()),
            lanes,
            limits: Arc::new(Limits::new()),
        })
    }

    /// The address an operator pastes into a configuration file.
    ///
    /// # Errors
    /// If the socket cannot report where it is.
    pub fn advertised(&self) -> Result<RelayAddress, RelaydError> {
        Ok(RelayAddress {
            address: self.endpoint.local_addr()?.to_string(),
            key: Some(self.device),
        })
    }

    /// Where the relay really bound, which is what `--listen 0.0.0.0:0` needs.
    ///
    /// # Errors
    /// If the socket cannot report it.
    pub fn local_address(&self) -> Result<SocketAddr, RelaydError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Serves until the endpoint is closed.
    pub async fn run(&self) {
        let sweeping = tokio::spawn(sweep(
            Arc::clone(&self.registry),
            Arc::clone(&self.limits),
        ));
        while let Some(incoming) = self.endpoint.accept().await {
            let address = canonical(incoming.remote_address());
            if !self.limits.admits(address) {
                judge(incoming);
                continue;
            }
            let serving = Serving {
                registry: Arc::clone(&self.registry),
                lanes: Arc::clone(&self.lanes),
            };
            tokio::spawn(async move {
                let connection = match incoming.await {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::debug!(%address, %error, "a handshake did not complete");
                        return;
                    }
                };
                if let Err(error) = serve(&serving, connection).await {
                    tracing::debug!(%address, %error, "a peer's connection ended");
                }
            });
        }
        sweeping.abort();
    }

    /// Closes the relay and waits for its connections to go.
    pub async fn shutdown(&self) {
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"relay closing");
        self.endpoint.wait_idle().await;
    }

    /// How many hosts are reachable, how many pairings are in progress, how many
    /// lane ends are live, and how many prefixes are being rate limited.
    #[must_use]
    pub fn counts(&self) -> Counts {
        Counts {
            registered: self.registry.registered(),
            pairing: self.registry.pairing(),
            lane_ends: self.lanes.len(),
            forwarded: self.lanes.carried(),
            watched: self.limits.watched(),
        }
    }
}

/// What a relay is currently holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Hosts reachable through this relay.
    pub registered: usize,
    /// Pairings in progress.
    pub pairing: usize,
    /// Live lane ends, which is twice the number of splices.
    pub lane_ends: usize,
    /// Bytes taken in over those lanes.
    pub forwarded: u64,
    /// Source prefixes the rate limiter is counting.
    pub watched: usize,
}

/// What to do with a connection attempt that is over its budget.
///
/// An address that has not been validated is sent a retry, which costs the relay
/// one packet and proves the address is real. One that has already been
/// validated and is still over budget is a repeat offender, and is ignored,
/// which sends nothing at all.
fn judge(incoming: quinn::Incoming) {
    if incoming.remote_address_validated() || !incoming.may_retry() {
        // It has proved its address and is still over budget. `ignore` sends
        // nothing at all, so there is not even a refusal for an attacker to
        // measure the relay by.
        incoming.ignore();
        return;
    }
    // One packet, and the address that gets it has to answer to keep going. A
    // spoofed source never does.
    if let Err(refused) = incoming.retry() {
        refused.into_incoming().ignore();
    }
}

/// Clears what would otherwise grow without bound.
async fn sweep(registry: Arc<Registry>, limits: Arc<Limits>) {
    let mut tick = tokio::time::interval(SWEEP);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let closed = registry.expire(Instant::now());
        if closed > 0 {
            tracing::info!(closed, "pairing mailboxes expired");
        }
        // Without this the keyed limiter's own map grows one entry per source
        // address forever, and the thing meant to stop a flood becomes one.
        limits.forget_idle();
    }
}
