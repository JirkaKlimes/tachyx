//! The two ends of the connection, and the settings both agree on.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{AckFrequencyConfig, Connection, Endpoint, IdleTimeout, TransportConfig, VarInt};
use rustls::crypto::CryptoProvider;
use tachyx_proto::control::{ClientHello, HostHello};

use crate::control::{ControlError, ControlReader, ControlWriter};
use crate::identity::{HostIdentity, IdentityError, PinStore, PinnedServer};
use crate::video::VideoSender;

/// The application protocol both ends announce.
///
/// The protocol version is not folded into this. A version mismatch belongs in
/// the hello, where it is reported by name, rather than in the handshake, where
/// it would surface as an opaque TLS alert.
const ALPN: &[u8] = b"tachyx";

/// The close code for a peer whose hello this build will not accept.
const REFUSED: VarInt = VarInt::from_u32(1);

/// Settings the defaults get wrong for a local network, and by large factors.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Bytes of queued video the transport may hold, at each end.
    ///
    /// Both ends evict the oldest queued datagram to make room, so a buffer
    /// that cannot hold a whole key frame does not slow the stream down: it
    /// shreds every key frame and the client never synchronises at all. See
    /// [`crate::video::video_send_buffer`].
    pub video_buffer: usize,
}

impl Tuning {
    /// Sizes the video buffer for a stream at `coded`.
    #[must_use]
    pub fn for_stream(coded: tachyx_proto::control::CodedSize) -> Self {
        Self {
            video_buffer: crate::video::video_send_buffer(coded),
        }
    }
}

/// The transport parameters for a link whose round trip is under a millisecond.
///
/// Every value here overrides a default that is wrong by a large factor. The
/// initial round trip estimate matters most: it defaults to 333 ms and it seeds
/// loss detection, so on a link whose real figure is under one millisecond a
/// lost packet would not be suspected for a third of a second.
fn transport_config(tuning: Tuning) -> TransportConfig {
    let mut config = TransportConfig::default();
    config
        .initial_rtt(Duration::from_millis(5))
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(5_000))))
        .keep_alive_interval(Some(Duration::from_millis(500)))
        // A 1500-byte path less twenty bytes of IPv4 and eight of UDP. Discovery
        // lowers it if the path turns out to be smaller.
        .initial_mtu(1472)
        // The bandwidth estimate the whole rate controller reads comes from
        // here. Only BBR reports a pacing rate; Cubic reports none.
        .congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()))
        .datagram_send_buffer_size(tuning.video_buffer)
        // The receive buffer discards the oldest queued datagram to make room,
        // exactly as the send buffer does, so a client whose buffer cannot hold
        // a key frame loses pieces of every key frame it is sent.
        .datagram_receive_buffer_size(Some(tuning.video_buffer))
        // The protocol opens one stream and no more.
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0));

    let mut acks = AckFrequencyConfig::default();
    acks.max_ack_delay(Some(Duration::from_millis(2)));
    config.ack_frequency_config(Some(acks));
    config
}

/// The cryptographic provider, named rather than defaulted.
///
/// quinn defaults to one provider and rustls to another, so taking both
/// defaults compiles both. Ring is the one named here: the other has an open
/// link failure on `aarch64-linux-android`, which is the client's main target.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The host end of the connection.
#[derive(Debug)]
pub struct Host {
    endpoint: Endpoint,
    tuning: Tuning,
}

impl Host {
    /// Binds a host at `address`, presenting `identity`.
    ///
    /// # Errors
    /// If the socket cannot be opened, or the identity will not make a TLS
    /// configuration.
    pub fn bind(
        address: SocketAddr,
        identity: &HostIdentity,
        tuning: Tuning,
    ) -> Result<Self, TransportError> {
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(identity.chain(), identity.key())?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let quic = QuicServerConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
        config.transport_config(Arc::new(transport_config(tuning)));
        Ok(Self {
            endpoint: Endpoint::server(config, address)?,
            tuning,
        })
    }

    /// The address the host actually bound, which is what to advertise.
    ///
    /// # Errors
    /// If the socket cannot report it.
    pub fn local_address(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for a client, completes the handshake, and exchanges hellos.
    ///
    /// The host's own hello goes out before the client's is read, so a client
    /// this build cannot talk to is told which version the host speaks rather
    /// than left with a connection that closed for no stated reason. For the
    /// same reason a refusal closes the connection with the refusal as its
    /// reason instead of dropping it.
    ///
    /// # Errors
    /// If the endpoint is closed, the connection fails, or the client's hello
    /// is not one this build understands.
    pub async fn accept(&self) -> Result<HostSession, TransportError> {
        let incoming = self.endpoint.accept().await.ok_or(TransportError::Closed)?;
        let connection = incoming.await?;
        // The client opens the stream, so the host waits for it rather than
        // racing to open one of its own.
        let (send, recv) = connection.accept_bi().await?;
        let mut to_client = ControlWriter::new(send);
        let mut from_client = ControlReader::new(recv);
        to_client.send(&HostHello::new()).await?;
        let hello = match from_client.recv_hello::<ClientHello>().await {
            Ok(hello) => hello,
            Err(refused) => {
                connection.close(REFUSED, refused.to_string().as_bytes());
                return Err(refused.into());
            }
        };
        Ok(HostSession {
            video: VideoSender::new(connection.clone(), self.tuning.video_buffer),
            to_client,
            from_client,
            hello,
            connection,
        })
    }

    /// The endpoint this host listens on, for its statistics and for a
    /// connection that is not a session: a peer that answers something no
    /// [`HostSession`] would answer, which is what proves a client reads a
    /// refusal.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Waits for every connection to close, so a shutdown is not a reset.
    pub async fn shutdown(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"host closing");
        self.endpoint.wait_idle().await;
    }

    /// The settings this host was built with.
    #[must_use]
    pub const fn tuning(&self) -> Tuning {
        self.tuning
    }
}

/// A live connection to one client.
#[derive(Debug)]
pub struct HostSession {
    /// The connection itself, for statistics and for closing it.
    pub connection: Connection,
    /// What the client said it can decode.
    pub hello: ClientHello,
    /// Encoded pictures out.
    pub video: VideoSender,
    /// Stream configuration and heartbeats out.
    pub to_client: ControlWriter,
    /// Input and key frame requests in.
    pub from_client: ControlReader,
}

/// The client end of the connection.
#[derive(Debug)]
pub struct Client {
    endpoint: Endpoint,
}

impl Client {
    /// Builds a client that trusts the hosts in `pins`, and any host it has not
    /// met before.
    ///
    /// # Errors
    /// If a socket cannot be opened, or this build cannot negotiate TLS 1.3.
    pub fn new(pins: Arc<PinStore>, tuning: Tuning) -> Result<Self, TransportError> {
        let provider = provider();
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServer::new(pins, provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let quic = QuicClientConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic));
        config.transport_config(Arc::new(transport_config(tuning)));

        // Unspecified address and port: the client opens the connection, so
        // nothing has to reach it at a known one.
        let mut endpoint = Endpoint::client((std::net::Ipv6Addr::UNSPECIFIED, 0).into())
            .or_else(|_| Endpoint::client((std::net::Ipv4Addr::UNSPECIFIED, 0).into()))?;
        endpoint.set_default_client_config(config);
        Ok(Self { endpoint })
    }

    /// The endpoint this client dials from, for its statistics, its address,
    /// and for a connection that is not a session: a peer that wants to say
    /// something no [`ClientSession`] would say, which is what proves the host
    /// refuses it.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Connects to the host at `address`, which is pinned under `host`.
    ///
    /// `host` names the peer for the purpose of pinning. It is not resolved and
    /// need not resolve: a host on a local network is found by discovery and
    /// reached by address, and what identifies it is its key.
    ///
    /// # Errors
    /// If the connection fails, or the host presents a key other than the one
    /// pinned for that name.
    pub async fn connect(
        &self,
        address: SocketAddr,
        host: &str,
        hello: &ClientHello,
    ) -> Result<ClientSession, TransportError> {
        // The name goes to quinn as written. Turning it into a `ServerName`
        // here and back into a string for quinn produced the debug form of an
        // address for every host named by one, which quinn then refused: a
        // client that dials an address it was told, which is every client on a
        // local network, could not connect at all.
        let connection = match self.endpoint.connect(address, host) {
            Ok(connecting) => connecting.await?,
            Err(quinn::ConnectError::InvalidServerName(_)) => {
                return Err(TransportError::UnusableHostName);
            }
            Err(refused) => return Err(refused.into()),
        };
        let (send, recv) = connection.open_bi().await?;
        let mut to_host = ControlWriter::new(send);
        // The stream reaches the host only when something is written on it, so
        // the client speaks first and the host answers.
        to_host.send(hello).await?;
        let mut from_host = ControlReader::new(recv);
        // A host that will not have this client closes the connection with the
        // reason attached, and closing discards stream data the peer has not
        // acknowledged. So the greeting usually loses the race and the close
        // reason is what a refused client actually reads. Both paths end in a
        // sentence naming the fault; neither ends in silence or a parse error.
        let greeting = match from_host.recv_hello::<HostHello>().await {
            Ok(greeting) => greeting,
            Err(unread) => {
                return Err(match connection.close_reason() {
                    Some(quinn::ConnectionError::ApplicationClosed(refusal)) => {
                        TransportError::Refused {
                            reason: String::from_utf8_lossy(&refusal.reason).into_owned(),
                        }
                    }
                    _ => unread.into(),
                });
            }
        };
        Ok(ClientSession {
            connection,
            greeting,
            to_host,
            from_host,
        })
    }
}

/// A live connection to the host.
#[derive(Debug)]
pub struct ClientSession {
    /// The connection itself, for statistics and for closing it.
    pub connection: Connection,
    /// What the host said before it read the hello. Its version has already
    /// been checked against this build's.
    pub greeting: HostHello,
    /// Input and key frame requests out.
    pub to_host: ControlWriter,
    /// Stream configuration and heartbeats in.
    pub from_host: ControlReader,
}

/// Why a connection could not be made or kept.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The socket could not be opened or read.
    #[error("the socket could not be used")]
    Socket(#[from] std::io::Error),
    /// The identity could not be settled.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The TLS configuration was refused.
    #[error("the TLS configuration was refused")]
    Tls(#[from] rustls::Error),
    /// This build cannot negotiate TLS 1.3, which QUIC requires.
    #[error("this build cannot negotiate TLS 1.3, which QUIC requires")]
    NoTls13,
    /// The connection could not be started.
    #[error("the connection could not be started")]
    Connect(#[from] quinn::ConnectError),
    /// The connection ended.
    #[error("the connection ended")]
    Connection(#[from] quinn::ConnectionError),
    /// The host closed the connection rather than opening a session, and said
    /// why. Its words, not ours: a version mismatch reads as a version
    /// mismatch and not as a failed read.
    #[error("the host refused the connection: {reason}")]
    Refused {
        /// What the host attached to the close.
        reason: String,
    },
    /// The control stream stopped carrying messages.
    #[error(transparent)]
    Control(#[from] ControlError),
    /// The endpoint has been closed.
    #[error("the endpoint is closed")]
    Closed,
    /// A name TLS cannot carry.
    #[error("the host name cannot be put in a TLS handshake")]
    UnusableHostName,
}
