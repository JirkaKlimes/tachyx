//! The peer's half of a relay: one connection, kept up.
//!
//! One QUIC connection to the relay, ALPN `tachyx-relay`, on the **same**
//! endpoint the sessions run on. quinn tells its own connections apart by
//! connection identifier, so the relay connection, the punch and the session
//! share one UDP port with nothing to write. That is what makes the address the
//! relay observes the address the peer will be reached at.
//!
//! The relay presents a certificate and it is checked against the key inside its
//! address. That key travels in the address because a relay address is
//! configuration a person pastes once, and pinning on first sight would leave
//! that one moment undefended for no gain.
//!
//! A host whose relay connection is gone is unreachable, so the connection comes
//! back on its own, with a backoff, and re-registers when it does.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use quinn::{Connection, Endpoint};
use tachyx_crypto::{DeviceId, DeviceKey, Nameplate, RelayAddress};
use rustls::client::danger::ServerCertVerifier;
use tachyx_transport::{AnyDevice, ControlReader, ControlWriter, Pinned, SERVER_NAME};
use tokio::sync::{broadcast, mpsc, watch, Mutex as AsyncMutex};

use crate::proto::{
    Hello, Offer, PeerHello, PeerMessage, Refusal, RelayHello, RelayMessage, ALPN, MAX_MESSAGE,
};
use crate::socket::{Routes, RoutingSocket};
use crate::RelayError;

/// How often the relay connection sends a keepalive.
///
/// A NAT mapping should be refreshed at least every 55 seconds. Twenty is the
/// safe operating point and is nowhere near that edge; going close to it trades
/// a handful of packets an hour for a mapping that dies while a host believes it
/// is reachable.
const KEEPALIVE: Duration = Duration::from_secs(20);

/// How long the relay connection may be silent before it is treated as gone.
///
/// Three keepalives. Shorter would drop a connection that lost one packet.
const RELAY_IDLE_MS: u32 = 60_000;

/// The first wait after the relay connection drops.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);

/// The longest wait between attempts.
///
/// A host that is unreachable for half a minute is already a fault somebody will
/// notice, and waiting longer than that only makes the recovery slower.
const LAST_BACKOFF: Duration = Duration::from_secs(30);

/// How long a request that expects an answer waits before it gives up.
const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// Refusals held for waiters that have not polled yet.
const REFUSAL_DEPTH: usize = 16;

/// What this peer wants the relay for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Be reachable under this device's identifier. The connection is
    /// registered as soon as it is up, and again after every reconnection.
    Host,
    /// Only reach somebody. Nothing is registered, so nothing can be routed to
    /// this peer.
    Client,
}

/// A lane the relay opened, already routed.
#[derive(Debug, Clone, Copy)]
pub struct Lane {
    /// Who is at the other end.
    pub peer: DeviceId,
    /// The address quinn knows that peer by. Sending to it wraps and forwards.
    pub known_as: SocketAddr,
}

/// Everything one connection to the relay hands upward.
#[derive(Debug)]
struct Inbox {
    nameplates: mpsc::UnboundedSender<Nameplate>,
    mail: mpsc::UnboundedSender<Vec<u8>>,
    offers: mpsc::UnboundedSender<Offer>,
    answers: mpsc::UnboundedSender<Offer>,
    lanes: mpsc::UnboundedSender<Lane>,
    /// Broadcast rather than queued, because the protocol's refusal does not say
    /// which request it answers, so every waiter has to see every refusal and
    /// decide for itself.
    refusals: broadcast::Sender<Refusal>,
}

/// What a caller reads.
#[derive(Debug)]
struct Outbox {
    nameplates: AsyncMutex<mpsc::UnboundedReceiver<Nameplate>>,
    mail: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    offers: AsyncMutex<mpsc::UnboundedReceiver<Offer>>,
    answers: AsyncMutex<mpsc::UnboundedReceiver<Offer>>,
    lanes: AsyncMutex<mpsc::UnboundedReceiver<Lane>>,
}

/// A relay connection that comes back on its own.
///
/// Dropping this stops the task that keeps it up.
#[derive(Debug)]
pub struct RelayClient {
    shared: Arc<Shared>,
    outbox: Outbox,
    keeper: tokio::task::JoinHandle<()>,
}

/// What the connection task and the callers share.
struct Shared {
    relay: RelayAddress,
    role: Role,
    key: Arc<DeviceKey>,
    endpoint: Endpoint,
    socket: Arc<RoutingSocket>,
    routes: Arc<Routes>,
    inbox: Inbox,
    /// Where the relay is, once a name has been resolved to an address.
    at: watch::Sender<Option<SocketAddr>>,
    /// The address the relay says it sees this peer at.
    observed: watch::Sender<Option<SocketAddr>>,
    /// `None` while there is no connection, which is what tells a caller to ask
    /// again rather than queue a request into nothing.
    out: Mutex<Option<mpsc::UnboundedSender<PeerMessage>>>,
    /// The live connection, kept so that its round trip can be read.
    ///
    /// quinn measures that from its own acknowledgements, so it is the real path
    /// to the relay and costs no message of ours. `status` and `doctor` are what
    /// read it.
    live: Mutex<Option<Connection>>,
}

/// Written by hand because a derived one would print this device's own key.
impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("relay", &self.relay)
            .field("role", &self.role)
            .field("device", &self.key.id())
            .finish_non_exhaustive()
    }
}

impl RelayClient {
    /// Opens a connection to `relay` and keeps it open.
    ///
    /// The connection is made in the background, so this returns before the
    /// relay has answered. [`RelayClient::wait_observed`] is what waits for it.
    #[must_use]
    pub fn open(
        relay: RelayAddress,
        role: Role,
        key: Arc<DeviceKey>,
        endpoint: Endpoint,
        socket: Arc<RoutingSocket>,
    ) -> Self {
        let (nameplates, nameplates_out) = mpsc::unbounded_channel();
        let (mail, mail_out) = mpsc::unbounded_channel();
        let (offers, offers_out) = mpsc::unbounded_channel();
        let (answers, answers_out) = mpsc::unbounded_channel();
        let (lanes, lanes_out) = mpsc::unbounded_channel();
        let (refusals, _) = broadcast::channel(REFUSAL_DEPTH);
        let routes = Arc::clone(socket.routes());
        let shared = Arc::new(Shared {
            relay,
            role,
            key,
            endpoint,
            socket,
            routes,
            inbox: Inbox {
                nameplates,
                mail,
                offers,
                answers,
                lanes,
                refusals,
            },
            at: watch::Sender::new(None),
            observed: watch::Sender::new(None),
            out: Mutex::new(None),
            live: Mutex::new(None),
        });
        Self {
            keeper: tokio::spawn(keep(Arc::clone(&shared))),
            shared,
            outbox: Outbox {
                nameplates: AsyncMutex::new(nameplates_out),
                mail: AsyncMutex::new(mail_out),
                offers: AsyncMutex::new(offers_out),
                answers: AsyncMutex::new(answers_out),
                lanes: AsyncMutex::new(lanes_out),
            },
        }
    }

    /// Which device this relay must be, when the operator pinned one.
    #[must_use]
    pub fn device(&self) -> Option<DeviceId> {
        self.shared.relay.key
    }

    /// Where the relay is, once its name has been resolved.
    #[must_use]
    pub fn address(&self) -> Option<SocketAddr> {
        *self.shared.at.borrow()
    }

    /// The round trip to the relay, as quinn measured it.
    ///
    /// `None` while there is no connection.
    #[must_use]
    pub fn round_trip(&self) -> Option<Duration> {
        self.shared
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(Connection::rtt)
    }

    /// The address the relay last said it saw this peer at.
    #[must_use]
    pub fn observed(&self) -> Option<SocketAddr> {
        *self.shared.observed.borrow()
    }

    /// Waits until the relay has said where it sees this peer.
    ///
    /// That message is sent unprompted as soon as the connection is up, so this
    /// is also how a caller waits for the connection itself.
    ///
    /// # Errors
    /// If nothing arrives inside `patience`.
    pub async fn wait_observed(&self, patience: Duration) -> Result<SocketAddr, RelayError> {
        let mut seen = self.shared.observed.subscribe();
        let waiting = async {
            loop {
                if let Some(address) = *seen.borrow_and_update() {
                    return Some(address);
                }
                if seen.changed().await.is_err() {
                    return None;
                }
            }
        };
        match tokio::time::timeout(patience, waiting).await {
            Ok(Some(address)) => Ok(address),
            Ok(None) => Err(RelayError::Offline),
            Err(_) => Err(RelayError::Silent),
        }
    }

    /// Opens a pairing mailbox.
    ///
    /// # Errors
    /// If the relay is not connected, refuses, or does not answer.
    pub async fn allocate(&self) -> Result<Nameplate, RelayError> {
        let mut refusals = self.shared.inbox.refusals.subscribe();
        self.shared.send(PeerMessage::Allocate)?;
        let mut nameplates = self.outbox.nameplates.lock().await;
        answer(&mut refusals, nameplates.recv()).await
    }

    /// Enters an open mailbox as its second party.
    ///
    /// There is no answer to this: the relay says nothing when it works, and a
    /// mailbox that is not there is refused when the first message is sent to
    /// it. So the refusal arrives at [`RelayClient::next_mail`].
    ///
    /// # Errors
    /// If the relay is not connected.
    pub fn join(&self, nameplate: Nameplate) -> Result<(), RelayError> {
        self.shared.send(PeerMessage::Join { nameplate })
    }

    /// Sends one pairing flight to the other party of the mailbox.
    ///
    /// # Errors
    /// If the relay is not connected.
    pub fn mail(&self, body: Vec<u8>) -> Result<(), RelayError> {
        self.shared.send(PeerMessage::Mail { body })
    }

    /// Waits for the other party's flight.
    ///
    /// # Errors
    /// If the relay refuses, or nothing arrives inside `patience`.
    pub async fn next_mail(&self, patience: Duration) -> Result<Vec<u8>, RelayError> {
        let mut refusals = self.shared.inbox.refusals.subscribe();
        let mut mail = self.outbox.mail.lock().await;
        tokio::time::timeout(patience, answer(&mut refusals, mail.recv()))
            .await
            .map_err(|_| RelayError::Silent)?
    }

    /// Closes this peer's mailbox now.
    ///
    /// # Errors
    /// If the relay is not connected.
    pub fn release(&self) -> Result<(), RelayError> {
        self.shared.send(PeerMessage::Release)
    }

    /// Asks the relay to carry `offer` to the host it names.
    ///
    /// # Errors
    /// If the relay is not connected.
    pub fn ask(&self, offer: Offer) -> Result<(), RelayError> {
        self.shared.send(PeerMessage::Connect { offer })
    }

    /// Answers somebody who asked.
    ///
    /// # Errors
    /// If the relay is not connected.
    pub fn reply(&self, offer: Offer) -> Result<(), RelayError> {
        self.shared.send(PeerMessage::Answer { offer })
    }

    /// Waits for somebody to ask for this host.
    ///
    /// The offer is **not** verified here. That is the caller's, because
    /// verifying it is what decides whether a packet is sent to any address it
    /// names, and that decision belongs to whatever holds the paired list.
    ///
    /// # Errors
    /// If the relay connection is gone.
    pub async fn next_offer(&self) -> Result<Offer, RelayError> {
        self.outbox
            .offers
            .lock()
            .await
            .recv()
            .await
            .ok_or(RelayError::Offline)
    }

    /// Waits for the host's answer.
    ///
    /// # Errors
    /// If the relay refuses, or nothing arrives inside `patience`.
    pub async fn next_answer(&self, patience: Duration) -> Result<Offer, RelayError> {
        let mut refusals = self.shared.inbox.refusals.subscribe();
        let mut answers = self.outbox.answers.lock().await;
        tokio::time::timeout(patience, answer(&mut refusals, answers.recv()))
            .await
            .map_err(|_| RelayError::Silent)?
    }

    /// Asks for a relayed path to `peer` and waits for it.
    ///
    /// The lane is already routed by the time this returns, and this peer has
    /// already proved its own address to the relay.
    ///
    /// # Errors
    /// If the relay is not connected, refuses, or does not answer.
    pub async fn splice(&self, peer: DeviceId) -> Result<Lane, RelayError> {
        let mut refusals = self.shared.inbox.refusals.subscribe();
        self.shared.send(PeerMessage::Splice { peer })?;
        let mut lanes = self.outbox.lanes.lock().await;
        let lane: Lane = tokio::time::timeout(
            REPLY_DEADLINE,
            answer::<Lane, _>(&mut refusals, lanes.recv()),
        )
        .await
        .map_err(|_| RelayError::Silent)??;
        if lane.peer == peer {
            Ok(lane)
        } else {
            Err(RelayError::NotForUs)
        }
    }

    /// Waits for a lane somebody else asked for.
    ///
    /// # Errors
    /// If the relay connection is gone.
    pub async fn next_lane(&self) -> Result<Lane, RelayError> {
        self.outbox
            .lanes
            .lock()
            .await
            .recv()
            .await
            .ok_or(RelayError::Offline)
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        self.keeper.abort();
    }
}

/// Either the reply that was expected, or the refusal that came instead.
async fn answer<T, F>(
    refusals: &mut broadcast::Receiver<Refusal>,
    reply: F,
) -> Result<T, RelayError>
where
    F: std::future::Future<Output = Option<T>>,
{
    tokio::select! {
        biased;
        got = reply => got.ok_or(RelayError::Offline),
        refused = refusals.recv() => Err(match refused {
            Ok(refusal) => RelayError::Refused(refusal),
            Err(_) => RelayError::Offline,
        }),
    }
}

impl Shared {
    /// Puts one message on the control stream, or says there is none.
    fn send(&self, message: PeerMessage) -> Result<(), RelayError> {
        let out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        out.as_ref()
            .ok_or(RelayError::Offline)?
            .send(message)
            .map_err(|_| RelayError::Offline)
    }
}

/// Keeps one connection to the relay up for as long as the client lives.
async fn keep(shared: Arc<Shared>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        match session(&shared).await {
            Ok(()) => {
                tracing::info!(relay = %shared.relay.address, "the relay connection closed");
                backoff = FIRST_BACKOFF;
            }
            Err(error) => {
                tracing::warn!(relay = %shared.relay.address, %error, "the relay connection failed");
            }
        }
        {
            let mut out = shared
                .out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *out = None;
        }
        {
            let mut live = shared
                .live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *live = None;
        }
        shared.observed.send_replace(None);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(LAST_BACKOFF);
    }
}

/// One connection to the relay, from the handshake to whatever ends it.
async fn session(shared: &Arc<Shared>) -> Result<(), RelayError> {
    let at = resolve(&shared.relay.address).await?;
    // `send_replace` rather than `send`: a watch channel with no receiver yet
    // refuses a `send` and keeps the old value, and the first thing that ever
    // subscribes here does so after the connection is already up.
    shared.at.send_replace(Some(at));
    shared.routes.note_relay(at);

    let connection = dial(shared, at).await?;
    {
        let mut live = shared
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live = Some(connection.clone());
    }
    let (send, recv) = connection.open_bi().await.map_err(transport)?;
    let mut writer = ControlWriter::new(send);
    let mut reader = ControlReader::new(recv);
    writer.send(&PeerHello::new()).await.map_err(transport)?;
    let _: RelayHello = RelayHello::decode(reader.bytes().await.map_err(transport)?)?;

    let (out, mut queued) = mpsc::unbounded_channel();
    if shared.role == Role::Host {
        // Sent before the sender is published, so a reconnection is registered
        // before anything else this peer wants to say.
        out.send(PeerMessage::Register)
            .map_err(|_| RelayError::Offline)?;
    }
    {
        let mut held = shared
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = Some(out);
    }
    tracing::info!(relay = %shared.relay.address, ?at, role = ?shared.role, "connected to a relay");

    loop {
        tokio::select! {
            outgoing = queued.recv() => match outgoing {
                Some(message) => writer.send(&message).await.map_err(transport)?,
                None => return Ok(()),
            },
            incoming = reader.bytes() => {
                let bytes = incoming.map_err(transport)?;
                if bytes.len() > MAX_MESSAGE {
                    return Err(RelayError::Malformed);
                }
                let message: RelayMessage =
                    postcard::from_bytes(bytes).map_err(|_| RelayError::Malformed)?;
                deliver(shared, at, message);
            }
        }
    }
}

/// Hands one message from the relay to whoever is waiting for it.
fn deliver(shared: &Arc<Shared>, at: SocketAddr, message: RelayMessage) {
    match message {
        RelayMessage::Observed { address } => {
            shared.observed.send_replace(Some(address));
        }
        RelayMessage::Nameplate { nameplate } => {
            let _ = shared.inbox.nameplates.send(nameplate);
        }
        RelayMessage::Mail { body } => {
            let _ = shared.inbox.mail.send(body);
        }
        RelayMessage::Offered { offer } => {
            let _ = shared.inbox.offers.send(offer);
        }
        RelayMessage::Answered { offer } => {
            let _ = shared.inbox.answers.send(offer);
        }
        RelayMessage::Lane { token, peer } => {
            // Routed and proved before anybody is told, so the first packet
            // through the lane is a session's and not a probe's.
            let known_as = shared.routes.relayed(at, token);
            shared.socket.probe(at, token);
            let _ = shared.inbox.lanes.send(Lane { peer, known_as });
        }
        RelayMessage::Refused { what } => {
            tracing::debug!(relay = %shared.relay.address, refusal = %what, "the relay refused");
            let _ = shared.inbox.refusals.send(what);
        }
    }
}

/// One QUIC connection to the relay, whose key must be the one in its address.
///
/// The configuration is built here rather than borrowed from the session's,
/// because two things differ and both matter: the ALPN says this is a relay
/// connection and not a desktop, and the keepalive is what holds a NAT mapping
/// open for the hours a host sits idle.
async fn dial(shared: &Arc<Shared>, at: SocketAddr) -> Result<Connection, RelayError> {
    let provider = tachyx_transport::provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| RelayError::Offline)?
        .dangerous()
        .with_custom_certificate_verifier(if let Some(key) = shared.relay.key {
            Arc::new(Pinned::new(key, provider)) as Arc<dyn ServerCertVerifier>
        } else {
            // The operator wrote the address without a key. The session stays
            // end to end either way; what an interceptor of this address gains
            // is the metadata and the power to force the relayed path, and the
            // log says so where the choice takes effect rather than only where
            // it was made.
            tracing::warn!(
                relay = %shared.relay.address,
                "this relay is not pinned: anyone who can intercept its address can be it, \
                 watch who this device reaches, and force the relayed path"
            );
            Arc::new(AnyDevice::new(provider)) as Arc<dyn ServerCertVerifier>
        })
        .with_client_auth_cert(vec![shared.key.certificate().map_err(transport)?], shared.key.private_key())
        .map_err(|_| RelayError::Offline)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).map_err(|_| RelayError::Offline)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));

    let mut limits = quinn::TransportConfig::default();
    limits
        .keep_alive_interval(Some(KEEPALIVE))
        .max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
            RELAY_IDLE_MS,
        ))))
        // The relay is across the internet, so this starts at what a tailnet or
        // a relayed hop carries and lets discovery raise it. A packet of the
        // local network's size would be black-holed until it did.
        .initial_mtu(tachyx_transport::RELAYED_MTU)
        // One stream carries control and nothing else travels on this
        // connection: the splice is beside it, on the same port.
        .max_concurrent_bidi_streams(quinn::VarInt::from_u32(1))
        .max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    config.transport_config(Arc::new(limits));

    shared
        .endpoint
        .connect_with(config, at, SERVER_NAME)
        .map_err(transport)?
        .await
        .map_err(transport)
}

/// Turns what the operator wrote into one address to dial.
async fn resolve(address: &str) -> Result<SocketAddr, RelayError> {
    if let Ok(socket) = address.parse() {
        return Ok(socket);
    }
    tokio::net::lookup_host(address)
        .await
        .ok()
        .and_then(|mut found| found.next())
        .ok_or(RelayError::Offline)
}

/// Anything the transport reports, as this crate's error.
fn transport(error: impl Into<tachyx_transport::TransportError>) -> RelayError {
    RelayError::Transport(error.into())
}
