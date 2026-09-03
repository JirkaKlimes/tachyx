//! Punch first, and fall back to the relay when the punch does not work.
//!
//! **Only the client makes the real connection.** The host punches by starting a
//! connection of its own toward each of the client's candidates and abandoning
//! it: those Initials open the host's NAT mapping, and the client's endpoint
//! carries no server configuration, so it discards them rather than answering.
//! Racing two real connections and closing the loser was the alternative, and it
//! costs a second handshake, a rule for which side wins, and a window in which
//! both are live. One direction is one code path.
//!
//! The host verifies the offer **before it sends one packet to any address the
//! offer names**. That is what the signature is for: an unsigned offer would make
//! every relay a reflector, because a stranger could name a victim's address and
//! have the host spray QUIC Initials at it.
//!
//! Two seconds without a direct connection and the client asks for a splice. The
//! session through it is the same session in the strongest sense available: the
//! same `Connection`, the same ALPN, the same certificates, the same key
//! comparison and the same congestion controller.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use quinn::Endpoint;
use tachyx_crypto::{DeviceId, DeviceKey, PairedClients, Signature};
use tachyx_proto::control::ClientHello;
use tachyx_transport::{client_config, Client, ClientSession, Tuning, SERVER_NAME};
use tokio::sync::watch;

use crate::client::RelayClient;
use crate::proto::{Offer, MAX_CANDIDATES};
use crate::socket::Routes;
use crate::RelayError;

/// How long the client punches before it asks for a relay.
///
/// Two seconds. A punch that works completes in a round trip or three; one that
/// has not worked by now is behind a NAT that will not let it.
pub const PUNCH_BUDGET: Duration = Duration::from_secs(2);

/// The shortest a single candidate is given.
///
/// A handful of candidates would otherwise divide the budget into slices too
/// short for one round trip on a slow path.
const CANDIDATE_FLOOR: Duration = Duration::from_millis(400);

/// How long the host keeps punching when nothing arrives.
const PUNCH_LIFETIME: Duration = Duration::from_secs(3);

/// How long a client waits for the host to answer its offer.
///
/// The host has to verify a signature and look in a file. Five seconds covers a
/// relay that is slow and a host that is busy, and it is short enough that a
/// host which is not there is reported rather than waited on.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(5);

/// Whether the session that opened runs on the wire or through a relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// Straight to the peer. The punch worked.
    Direct,
    /// Through the relay, nine bytes a packet.
    Relayed,
}

/// Seconds since the Unix epoch, and zero on a clock set before it.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The addresses to offer: this peer's own view of itself first, then what the
/// relay observed, with duplicates and the overflow dropped.
fn candidates(own: &[SocketAddr], observed: SocketAddr) -> Vec<SocketAddr> {
    let mut offered = Vec::with_capacity(own.len() + 1);
    for address in own.iter().copied().chain(std::iter::once(observed)) {
        if !offered.contains(&address) {
            offered.push(address);
        }
    }
    offered.truncate(MAX_CANDIDATES);
    offered
}

/// The client's half: reach a host, direct if it can and relayed if it must.
#[derive(Debug)]
pub struct ClientRendezvous {
    key: Arc<DeviceKey>,
    relay: Arc<RelayClient>,
    routes: Arc<Routes>,
    client: Client,
}

impl ClientRendezvous {
    /// Reaches hosts through `relay`, dialling from `client`.
    ///
    /// `client` must be built on the same endpoint the relay connection runs on,
    /// and `routes` must be that endpoint's table. Both are what make the address
    /// the relay observed the address the host will punch toward.
    #[must_use]
    pub const fn new(
        key: Arc<DeviceKey>,
        relay: Arc<RelayClient>,
        routes: Arc<Routes>,
        client: Client,
    ) -> Self {
        Self {
            key,
            relay,
            routes,
            client,
        }
    }

    /// The dialling end, for its endpoint and its statistics.
    #[must_use]
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Opens a session with `host`.
    ///
    /// `own` is this peer's own view of where it can be reached, which is
    /// whatever discovery found. The relay's observation is added to it.
    ///
    /// # Errors
    /// If the relay is not up, the host does not answer, the host's answer is
    /// not signed by the host, or neither the punch nor the splice opens a
    /// session.
    pub async fn open(
        &self,
        host: DeviceId,
        own: &[SocketAddr],
        hello: &ClientHello,
    ) -> Result<(ClientSession, Path), RelayError> {
        let observed = self.relay.wait_observed(ANSWER_DEADLINE).await?;
        let offer = Offer::sign(&self.key, host, candidates(own, observed), now())?;
        self.relay.ask(offer)?;
        let answer = self.relay.next_answer(ANSWER_DEADLINE).await?;
        // The host's answer is checked exactly as the host checked ours: an
        // answer forwarded by a relay is a relay's word until its signature says
        // otherwise.
        answer.verify(now())?;
        if answer.from != host || answer.to != self.key.id() {
            return Err(RelayError::NotForUs);
        }

        if let Some(session) = self.punch(host, &answer.candidates, hello).await {
            return Ok((session, Path::Direct));
        }

        let lane = self.relay.splice(host).await?;
        tracing::info!(%host, "the punch did not work, so the session is relayed");
        let session = self.client.connect(lane.known_as, host, hello).await;
        match session {
            Ok(session) => Ok((session, Path::Relayed)),
            Err(error) => {
                self.routes.forget(lane.known_as);
                Err(RelayError::Transport(error))
            }
        }
    }

    /// Tries each candidate in turn until the budget is spent.
    ///
    /// In turn rather than at once: two real connections to the same host would
    /// need a rule for which one wins, and the candidates a host offers are a
    /// handful, so the budget covers them.
    async fn punch(
        &self,
        host: DeviceId,
        candidates: &[SocketAddr],
        hello: &ClientHello,
    ) -> Option<ClientSession> {
        if candidates.is_empty() {
            return None;
        }
        let each =
            (PUNCH_BUDGET / u32::try_from(candidates.len()).unwrap_or(1)).max(CANDIDATE_FLOOR);
        let until = tokio::time::Instant::now() + PUNCH_BUDGET;
        for candidate in candidates {
            if tokio::time::Instant::now() >= until {
                break;
            }
            // A virtual address even for a direct path, so that the route under
            // a live session can be moved without quinn seeing a migration.
            let known_as = self.routes.direct(*candidate);
            let dialling = self.client.connect(known_as, host, hello);
            match tokio::time::timeout(each, dialling).await {
                Ok(Ok(session)) => return Some(session),
                Ok(Err(error)) => {
                    tracing::debug!(%candidate, %error, "that candidate did not answer");
                }
                Err(_) => tracing::debug!(%candidate, "that candidate did not answer in time"),
            }
            self.routes.forget(known_as);
        }
        None
    }
}

/// Which devices have connected, so a punch stops as soon as it has worked.
///
/// Whatever accepts sessions calls [`Arrivals::note`]. Without it a host would
/// keep sending Initials at a client that is already talking to it, for the
/// whole three seconds.
#[derive(Debug)]
pub struct Arrivals(watch::Sender<Option<DeviceId>>);

impl Default for Arrivals {
    fn default() -> Self {
        Self::new()
    }
}

impl Arrivals {
    /// Nothing has arrived yet.
    #[must_use]
    pub fn new() -> Self {
        Self(watch::Sender::new(None))
    }

    /// Records that `peer` is connected.
    ///
    /// `send_replace` rather than `send`: a watch channel with no receiver yet
    /// refuses a `send` and keeps the old value, and a session can be accepted
    /// before the punch it answers has started waiting.
    pub fn note(&self, peer: DeviceId) {
        self.0.send_replace(Some(peer));
    }

    /// Waits until `peer` is recorded.
    async fn wait(&self, peer: DeviceId) {
        let mut seen = self.0.subscribe();
        while *seen.borrow_and_update() != Some(peer) {
            if seen.changed().await.is_err() {
                // Nothing will record an arrival again, which is what a host
                // shutting down looks like. Stop punching.
                return;
            }
        }
    }
}

/// The host's half: answer offers, punch, and keep the lanes drained.
#[derive(Debug)]
pub struct HostRendezvous {
    key: Arc<DeviceKey>,
    clients: Arc<PairedClients>,
    relay: Arc<RelayClient>,
    endpoint: Endpoint,
    arrivals: Arc<Arrivals>,
    tuning: Tuning,
    seen: SeenOffers,
}

impl HostRendezvous {
    /// Answers for `key`, taking only the devices in `clients`.
    #[must_use]
    pub const fn new(
        key: Arc<DeviceKey>,
        clients: Arc<PairedClients>,
        relay: Arc<RelayClient>,
        endpoint: Endpoint,
        arrivals: Arc<Arrivals>,
        tuning: Tuning,
    ) -> Self {
        Self {
            key,
            clients,
            relay,
            endpoint,
            arrivals,
            tuning,
            seen: SeenOffers {
                inside: Mutex::new(Vec::new()),
            },
        }
    }

    /// Runs until the relay connection is gone for good.
    ///
    /// Both arms have to be read: an offer that is not answered is a client that
    /// cannot connect, and a lane that is not read is an unbounded queue.
    pub async fn serve(&self, own: &[SocketAddr]) -> RelayError {
        loop {
            let offer = tokio::select! {
                offer = self.relay.next_offer() => offer,
                lane = self.relay.next_lane() => match lane {
                    // The route was installed and the address proved before this
                    // arrived. Nothing else is left to do with it.
                    Ok(lane) => {
                        tracing::info!(peer = %lane.peer, "a relayed path is open");
                        continue;
                    }
                    Err(error) => Err(error),
                },
            };
            match offer {
                Ok(offer) => {
                    if let Err(error) = self.answer(&offer, own).await {
                        tracing::warn!(from = %offer.from, %error, "refused an offer");
                    }
                }
                Err(error) => return error,
            }
        }
    }

    /// Verifies one offer and, only if it stands up, answers and punches.
    ///
    /// Public because it is the decision this whole module exists to make, and
    /// a test that cannot reach it is a test of something else.
    ///
    /// # Errors
    /// If the offer is not signed by the device it names, is stale, names
    /// another host, or comes from a device this host has not paired with. In
    /// every one of those cases nothing has been sent to any address the offer
    /// carried.
    pub async fn answer(&self, offer: &Offer, own: &[SocketAddr]) -> Result<(), RelayError> {
        // In this order, and nothing above sends a packet: the signature, then
        // who it names, then whether that device is paired, then whether this
        // exact offer has been answered already. Only after all four does an
        // address the offer supplied reach a socket.
        offer.verify(now())?;
        if offer.to != self.key.id() || self.clients.get(offer.from).is_none() {
            return Err(RelayError::NotForUs);
        }
        if !self.seen.first_sight(offer, now()) {
            // A replay inside the freshness window. The signature is valid and
            // the signer is paired, so this is a relay repeating itself rather
            // than a stranger, and what refusing it protects is the punch: an
            // answered offer costs Initials at every address it names, and a
            // replayed one must not multiply them.
            return Err(RelayError::NotForUs);
        }
        let observed = self.relay.wait_observed(ANSWER_DEADLINE).await?;
        let mine = Offer::sign(&self.key, offer.from, candidates(own, observed), now())?;
        self.relay.reply(mine)?;
        self.spawn_punch(offer.from, offer.candidates.clone());
        Ok(())
    }

    /// Starts the throwaway connections that open this host's NAT mapping.
    fn spawn_punch(&self, peer: DeviceId, candidates: Vec<SocketAddr>) {
        let Ok(config) = client_config(&self.key, peer, self.tuning) else {
            return;
        };
        let endpoint = self.endpoint.clone();
        let arrivals = Arc::clone(&self.arrivals);
        tokio::spawn(async move {
            let punch = punch(&endpoint, &config, &candidates);
            tokio::select! {
                () = arrivals.wait(peer) => {}
                () = tokio::time::sleep(PUNCH_LIFETIME) => {}
            }
            drop(punch);
        });
    }
}

/// The offers already answered inside the freshness window.
///
/// An offer carries no nonce and needs none: its signature is unique to its
/// `(from, to, candidates, at)` and [`Offer::verify`] already bounds `at` to a
/// window. Remembering the signatures seen inside that window makes each signed
/// offer worth exactly one answer and one punch, which is what
/// `spec/relay.md` promises. Sixty-four bytes per live offer, pruned as the
/// window moves, so a chatty relay cannot grow it past what the window holds.
#[derive(Debug, Default)]
struct SeenOffers {
    inside: Mutex<Vec<(u64, Signature)>>,
}

impl SeenOffers {
    /// Whether this is the first sight of `offer`, recording it when it is.
    fn first_sight(&self, offer: &Offer, now: u64) -> bool {
        let mut inside = self
            .inside
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The prune bound matches the verify window: anything older would be
        // refused as stale before this set is consulted, so remembering it
        // buys nothing.
        inside.retain(|(at, _)| now.saturating_sub(*at) <= tachyx_relay_offer_window());
        if inside.iter().any(|(_, seen)| *seen == offer.signature) {
            return false;
        }
        inside.push((offer.at, offer.signature));
        true
    }
}

/// The freshness window, named once here rather than copied.
const fn tachyx_relay_offer_window() -> u64 {
    crate::proto::OFFER_WINDOW
}

/// Initials at every candidate, and nothing that expects an answer.
///
/// The far end is a client, whose endpoint carries no server configuration, so
/// quinn there discards these rather than answering. What they achieve is the
/// NAT mapping on **this** side, which is the whole of the host's part in a
/// punch.
#[must_use]
fn punch(endpoint: &Endpoint, config: &quinn::ClientConfig, candidates: &[SocketAddr]) -> Punch {
    let tasks = candidates
        .iter()
        .filter_map(|candidate| {
            endpoint
                .connect_with(config.clone(), *candidate, SERVER_NAME)
                .ok()
        })
        .map(|connecting| {
            // Awaited so that quinn keeps retransmitting the Initial for as long
            // as the punch lasts. It never completes: that is the point.
            tokio::spawn(async move {
                let _ = connecting.await;
            })
        })
        .collect();
    Punch { tasks }
}

/// The throwaway connections, abandoned when this is dropped.
#[derive(Debug)]
struct Punch {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Punch {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last], 47_989))
    }

    #[test]
    fn the_peers_own_view_comes_first_and_the_relays_observation_last() {
        let offered = candidates(&[address(1), address(2)], address(9));
        assert_eq!(offered, vec![address(1), address(2), address(9)]);
    }

    #[test]
    fn an_address_the_relay_confirms_is_offered_once() {
        assert_eq!(candidates(&[address(1)], address(1)), vec![address(1)]);
    }

    #[test]
    fn an_offer_never_names_more_addresses_than_a_host_will_punch_at() {
        let many: Vec<SocketAddr> = (0..64).map(address).collect();
        assert_eq!(candidates(&many, address(200)).len(), MAX_CANDIDATES);
    }

    #[test]
    fn a_replayed_offer_is_answered_once_inside_its_window() {
        use tachyx_crypto::DeviceKey;

        let signer = DeviceKey::from_seed(&[1u8; 32]).expect("a key");
        let to = DeviceKey::from_seed(&[2u8; 32]).expect("a key").id();
        let offer = crate::proto::Offer::sign(&signer, to, vec![address(1)], 1_000)
            .expect("an offer signs");
        let seen = SeenOffers::default();

        assert!(seen.first_sight(&offer, 1_000), "the first sight answers");
        assert!(
            !seen.first_sight(&offer, 1_010),
            "the same signature inside the window is a relay repeating itself, \
             and every repeat it earned would be a punch at every address the \
             offer names"
        );

        // Outside the window the set has let it go, which costs nothing:
        // `Offer::verify` refuses it as stale before this set is consulted.
        assert!(seen.first_sight(&offer, 1_000 + crate::proto::OFFER_WINDOW + 1));
    }

    #[test]
    fn the_seen_set_holds_only_what_the_window_holds() {
        use tachyx_crypto::DeviceKey;

        let signer = DeviceKey::from_seed(&[3u8; 32]).expect("a key");
        let to = DeviceKey::from_seed(&[4u8; 32]).expect("a key").id();
        let seen = SeenOffers::default();
        for at in 0..200u64 {
            let offer = crate::proto::Offer::sign(&signer, to, vec![address(1)], at)
                .expect("an offer signs");
            assert!(seen.first_sight(&offer, at));
        }
        let held = seen
            .inside
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert!(
            held <= usize::try_from(crate::proto::OFFER_WINDOW).expect("small") + 1,
            "{held} held: a chatty relay must not grow this past the window"
        );
    }
}
