//! The two ends of the connection, and the settings both agree on.
//!
//! Nothing in here binds a socket unless it is asked to. [`Host::bind`] and
//! [`Client::bind`] are the plain local-network path; [`Host::on_endpoint`] and
//! [`Client::on_endpoint`] take an endpoint somebody else built, which is what
//! `tachyx-relay` does when the socket underneath has to route. This crate does
//! not know relays exist, and it must not: the dependency runs the other way.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    AckFrequencyConfig, Connection, Endpoint, EndpointConfig, IdleTimeout, TokioRuntime,
    TransportConfig, VarInt,
};
use socket2::{Domain, Protocol, Socket, Type};
use tachyx_crypto::{CryptoError, DeviceId, DeviceKey, PairedClients};
use tachyx_proto::control::{ClientHello, HostHello};

use crate::audio::AudioSender;
use crate::control::{ControlError, ControlReader, ControlWriter};
use crate::identity::{peer_device, provider, Paired, Pinned, SERVER_NAME};
use crate::video::{VideoControl, VideoSender};

/// The application protocol both ends announce.
///
/// The protocol version is not folded into this. A version mismatch belongs in
/// the hello, where it is reported by name, rather than in the handshake, where
/// it would surface as an opaque TLS alert.
const ALPN: &[u8] = b"tachyx";

/// The close code for a peer whose hello this build will not accept.
const REFUSED: VarInt = VarInt::from_u32(1);

/// The starting packet size on a path this build believes is a local network.
///
/// A 1500-byte path less twenty bytes of IPv4 and eight of UDP. Discovery lowers
/// it if the path turns out to be smaller.
pub const LAN_MTU: u16 = 1472;

/// The starting packet size on a path that may be relayed or on a tailnet.
///
/// Such a path is 1280 bytes at the interface, and every packet of the local
/// network's size is then black-holed until discovery notices, which is a stall
/// at exactly the moment a session opens. Discovery raises this if the path
/// turns out to be larger, which costs nothing; starting too high costs the
/// first second of the session.
pub const RELAYED_MTU: u16 = 1200;

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
    /// The packet size to start at, before discovery has measured the path.
    ///
    /// [`LAN_MTU`] for a socket that sends straight out the wire, and
    /// [`RELAYED_MTU`] for one that may be wrapped and forwarded.
    pub initial_mtu: u16,
}

impl Tuning {
    /// Sizes the video buffer for a stream at `coded`, on a local network.
    #[must_use]
    pub fn for_stream(coded: tachyx_proto::control::CodedSize) -> Self {
        Self {
            video_buffer: crate::video::video_send_buffer(coded),
            initial_mtu: LAN_MTU,
        }
    }

    /// The same settings, for a path that may be relayed.
    #[must_use]
    pub const fn relayed(self) -> Self {
        Self {
            initial_mtu: RELAYED_MTU,
            ..self
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
        .initial_mtu(tuning.initial_mtu)
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

/// The receive and send buffer this transport asks the kernel for, in bytes.
///
/// A 1080p key frame is about ninety datagrams, released in a burst. Under GRO
/// the kernel hands the receiver several datagrams at once, so a receiver that
/// does not drain in time drops the tail of the burst. That shreds the key frame
/// and forces a re-sync. Four MiB holds many such bursts, so the drain margin
/// stops being the limit. This is hygiene, not a congestion control: it widens a
/// margin, it does not pace or retransmit.
///
/// The kernel clamps a request above `net.core.rmem_max` (and `wmem_max`). An
/// unprivileged process cannot exceed that ceiling, and this build does not use
/// `SO_RCVBUFFORCE`, which needs `CAP_NET_ADMIN`. So the grant may be smaller
/// than this. The kernel also reports a granted size as twice the value it
/// stored, so the logged figure reads as double the request when the request was
/// granted in full.
const SOCKET_BUFFER_TARGET: usize = 4 * 1024 * 1024;

/// Binds a UDP socket at `address`, with its receive and send buffers raised and
/// its packets marked as real time media.
///
/// Public because the socket a session runs on is not always bound here: a peer
/// that can be relayed binds one socket and puts a routing layer over it, and
/// that socket must carry the same hygiene as this one.
///
/// # Errors
/// If the socket cannot be opened or bound. A failure to raise a buffer is not
/// one of these: the stream works with whatever buffer the socket already has,
/// so that failure is logged and the socket is returned unchanged.
pub fn bind_socket(address: SocketAddr) -> Result<std::net::UdpSocket, TransportError> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    socket.bind(&address.into())?;
    raise_buffers(&socket);
    mark_priority(&socket, address);
    // quinn hands the socket to tokio, which needs it non-blocking.
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Binds an unspecified address, preferring a dual-stack IPv6 socket.
///
/// The dialling end opens the connection, so nothing has to reach it at a known
/// port. IPv6 is tried first, then IPv4 for a machine without it.
///
/// # Errors
/// If neither family can be bound.
pub fn bind_any() -> Result<std::net::UdpSocket, TransportError> {
    bind_socket((std::net::Ipv6Addr::UNSPECIFIED, 0).into())
        .or_else(|_| bind_socket((std::net::Ipv4Addr::UNSPECIFIED, 0).into()))
}

/// The differentiated services class the video is marked with.
///
/// Expedited Forwarding, the class real time media uses. A Wi-Fi access point
/// maps it to the voice access category and gives it airtime, where an unmarked
/// best effort flow of the same rate is throttled behind everything else and,
/// on a busy access point, starved until it stalls. Zoom and WebRTC mark their
/// media for exactly this reason. The value is the whole eight bit field, so
/// the six DSCP bits sit above the two that are always zero here.
const DSCP_EXPEDITED: u32 = 46 << 2;

/// Marks the socket's packets as real time media, so a Wi-Fi access point gives
/// them airtime instead of throttling them as a best effort flood.
///
/// Not fatal if it fails: the stream still works unmarked, this only wins it
/// priority on a contended link.
fn mark_priority(socket: &Socket, address: SocketAddr) {
    let marked = if address.is_ipv6() {
        socket.set_tclass_v6(DSCP_EXPEDITED)
    } else {
        socket.set_tos_v4(DSCP_EXPEDITED)
    };
    if let Err(err) = marked {
        tracing::warn!(?err, "could not mark the video as priority traffic");
    } else {
        tracing::info!(dscp = 46, "marked the socket as expedited real time media");
    }
}

/// Asks the kernel for [`SOCKET_BUFFER_TARGET`] on both buffers, then logs what
/// it granted, so an operator can see on-device what the kernel gave against
/// what was asked.
fn raise_buffers(socket: &Socket) {
    if let Err(err) = socket.set_recv_buffer_size(SOCKET_BUFFER_TARGET) {
        tracing::warn!(?err, "could not raise the UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(SOCKET_BUFFER_TARGET) {
        tracing::warn!(?err, "could not raise the UDP send buffer");
    }
    match (socket.recv_buffer_size(), socket.send_buffer_size()) {
        (Ok(recv), Ok(send)) => tracing::info!(
            requested = SOCKET_BUFFER_TARGET,
            recv,
            send,
            "UDP socket buffers set (the kernel reports about twice the stored size)"
        ),
        (recv, send) => {
            tracing::warn!(
                ?recv,
                ?send,
                "could not read back the UDP socket buffer sizes"
            );
        }
    }
}

/// The server configuration a host listens with: its own certificate, and a
/// verifier that takes only the devices it has paired with.
///
/// Public because a host that can be reached through a relay does not bind its
/// own endpoint. It hands this to whatever built the endpoint instead.
///
/// # Errors
/// If the certificate machinery refuses, or this build cannot negotiate TLS 1.3.
pub fn server_config(
    key: &DeviceKey,
    clients: Arc<PairedClients>,
    tuning: Tuning,
) -> Result<quinn::ServerConfig, TransportError> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(Paired::new(clients, provider)))
        .with_single_cert(vec![key.certificate()?], key.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// The client configuration for reaching exactly one host.
///
/// # Errors
/// If the certificate machinery refuses, or this build cannot negotiate TLS 1.3.
pub fn client_config(
    key: &DeviceKey,
    host: DeviceId,
    tuning: Tuning,
) -> Result<quinn::ClientConfig, TransportError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned::new(host, provider)))
        .with_client_auth_cert(vec![key.certificate()?], key.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// The host end of the connection.
#[derive(Debug)]
pub struct Host {
    endpoint: Endpoint,
    tuning: Tuning,
    /// What every hello announces about audio. See [`Host::with_audio`].
    audio: bool,
}

impl Host {
    /// Binds a host at `address`, presenting `key` and accepting `clients`.
    ///
    /// # Errors
    /// If the socket cannot be opened, or the key will not make a TLS
    /// configuration.
    pub fn bind(
        address: SocketAddr,
        key: &DeviceKey,
        clients: Arc<PairedClients>,
        tuning: Tuning,
    ) -> Result<Self, TransportError> {
        let config = server_config(key, clients, tuning)?;
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(config),
            bind_socket(address)?,
            Arc::new(TokioRuntime),
        )?;
        Ok(Self {
            endpoint,
            tuning,
            audio: false,
        })
    }

    /// Announces that this host can stream its own sound.
    ///
    /// Off unless a caller says otherwise, because whether a machine has a
    /// working capture path is something only the daemon that probed it knows,
    /// and a host that announced audio it cannot deliver would put a control in
    /// front of the user that does nothing.
    #[must_use]
    pub const fn with_audio(mut self, audio: bool) -> Self {
        self.audio = audio;
        self
    }

    /// Serves sessions on an endpoint somebody else built.
    ///
    /// `endpoint` must already carry a [`server_config`], because the endpoint's
    /// socket is shared with whatever built it: a relay connection and a session
    /// run on the same port, and quinn tells them apart by connection
    /// identifier.
    #[must_use]
    pub const fn on_endpoint(endpoint: Endpoint, tuning: Tuning) -> Self {
        Self {
            endpoint,
            tuning,
            audio: false,
        }
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
    /// The handshake has already refused any device this host has not paired
    /// with, so [`HostSession::peer`] names a device that is in the list. The
    /// host's own hello goes out before the client's is read, so a client this
    /// build cannot talk to is told which version the host speaks rather than
    /// left with a connection that closed for no stated reason. For the same
    /// reason a refusal closes the connection with the refusal as its reason
    /// instead of dropping it.
    ///
    /// # Errors
    /// If the endpoint is closed, the connection fails, or the client's hello
    /// is not one this build understands.
    pub async fn accept(&self) -> Result<HostSession, TransportError> {
        let incoming = self.endpoint.accept().await.ok_or(TransportError::Closed)?;
        let connection = incoming.await?;
        let peer = peer_device(&connection).ok_or(TransportError::UnknownPeer)?;
        // The client opens the stream, so the host waits for it rather than
        // racing to open one of its own.
        let (send, recv) = connection.accept_bi().await?;
        let mut to_client = ControlWriter::new(send);
        let mut from_client = ControlReader::new(recv);
        to_client.send(&HostHello::new(self.audio)).await?;
        let hello = match from_client.recv_hello::<ClientHello>().await {
            Ok(hello) => hello,
            Err(refused) => {
                connection.close(REFUSED, refused.to_string().as_bytes());
                return Err(refused.into());
            }
        };
        // The ceiling the client announced, until the session says what it
        // actually configured the encoder for. It is an over-estimate of the
        // real operating point, which is the safe direction: pacing above what
        // is being sent costs nothing, and pacing below it would throttle the
        // encoder from inside the transport.
        let (video, control) = VideoSender::new(
            connection.clone(),
            self.tuning.video_buffer,
            hello.capabilities.bitrate_ceiling,
        );
        // Taken here, while the video sender is still in hand. It is what ties
        // the two paths to one sequence and one set of release records, and the
        // session moves the video sender onto a thread of its own.
        let audio = video.audio();
        Ok(HostSession {
            connection,
            peer,
            hello,
            video,
            audio,
            control,
            to_client,
            from_client,
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
    /// Which device this is. It is in the paired list, because the handshake
    /// refused every device that is not, and it is what a caller looks
    /// [`Permissions`](tachyx_crypto::Permissions) up by.
    pub peer: DeviceId,
    /// What the client said it can decode.
    pub hello: ClientHello,
    /// Encoded pictures out.
    ///
    /// Moved into a thread of its own for the life of the session, because
    /// releasing a paced key frame is tens of milliseconds of waiting.
    pub video: VideoSender,
    /// Encoded sound out, on the same connection and past the pacer.
    ///
    /// A separate handle rather than a method on the video sender, because the
    /// video sender is on a thread that spends most of its time asleep between
    /// datagrams and sound cannot wait for it.
    pub audio: AudioSender,
    /// What the congestion controller drives that thread through.
    pub control: VideoControl,
    /// Stream configuration and heartbeats out.
    pub to_client: ControlWriter,
    /// Input and key frame requests in.
    pub from_client: ControlReader,
}

/// The client end of the connection.
#[derive(Debug)]
pub struct Client {
    endpoint: Endpoint,
    key: Arc<DeviceKey>,
    tuning: Tuning,
}

impl Client {
    /// Binds a client on an unspecified address, presenting `key`.
    ///
    /// # Errors
    /// If a socket cannot be opened.
    pub fn bind(key: Arc<DeviceKey>, tuning: Tuning) -> Result<Self, TransportError> {
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            None,
            bind_any()?,
            Arc::new(TokioRuntime),
        )?;
        Ok(Self {
            endpoint,
            key,
            tuning,
        })
    }

    /// Dials from an endpoint somebody else built.
    ///
    /// The endpoint carries no server configuration when it is only a client's,
    /// which is what makes it drop an incoming Initial rather than answer it.
    /// That is the property the relay's hole punch stands on.
    #[must_use]
    pub const fn on_endpoint(endpoint: Endpoint, key: Arc<DeviceKey>, tuning: Tuning) -> Self {
        Self {
            endpoint,
            key,
            tuning,
        }
    }

    /// The endpoint this client dials from, for its statistics, its address,
    /// and for a connection that is not a session: a peer that wants to say
    /// something no [`ClientSession`] would say, which is what proves the host
    /// refuses it.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// This client's own device.
    #[must_use]
    pub fn device(&self) -> DeviceId {
        self.key.id()
    }

    /// Connects to `host` at `address`.
    ///
    /// `address` is where to send packets; `host` is who must answer. The two
    /// are unrelated: an address is discovered, changes, and is not
    /// authenticated, and a device identifier is none of those things. A peer at
    /// that address presenting any other key ends the handshake.
    ///
    /// A host that will not have **this** device refuses a moment later than
    /// that, and there is nothing to be done about it: in TLS 1.3 the client's
    /// certificate travels in its second flight, so QUIC reports the handshake
    /// complete to the dialling end before the host has looked at it. The
    /// refusal then arrives as a closed connection on the first read, which is
    /// where it is reported.
    ///
    /// # Errors
    /// If the connection fails, the host presents another key, or the host
    /// refuses the session.
    pub async fn connect(
        &self,
        address: SocketAddr,
        host: DeviceId,
        hello: &ClientHello,
    ) -> Result<ClientSession, TransportError> {
        let config = client_config(&self.key, host, self.tuning)?;
        // The name is a constant, because nothing reads it. A self-signed
        // certificate says whatever its maker liked about names and dates, so
        // the key is the whole of the identity and the name is a field TLS
        // requires to be filled in.
        let connection = self
            .endpoint
            .connect_with(config, address, SERVER_NAME)?
            .await?;
        let peer = peer_device(&connection).ok_or(TransportError::UnknownPeer)?;
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
                    // A host that has not paired with this device refuses inside
                    // TLS instead, and closes with an alert. Report the close and
                    // not the read that failed behind it: "a read failed" says
                    // nothing, and the alert names the fault.
                    Some(closed) => TransportError::Connection(closed),
                    None => unread.into(),
                });
            }
        };
        Ok(ClientSession {
            connection,
            peer,
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
    /// Which device answered. It is the device that was asked for: the
    /// handshake refused every other one.
    pub peer: DeviceId,
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
    /// The device key would not produce a certificate, or the paired list would
    /// not open.
    #[error(transparent)]
    Identity(#[from] CryptoError),
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
    /// The handshake completed with no certificate to name the peer by, which
    /// these configurations do not allow and which therefore means the session
    /// cannot say who it is talking to.
    #[error("the peer presented no certificate to be named by")]
    UnknownPeer,
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
}
