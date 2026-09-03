//! Where this host looks for a relay, and the mailbox a pairing runs in.
//!
//! [`spec/relay.md`](../../../spec/relay.md) is the document. A host keeps one
//! connection to its home relay open so that a client can find it, and that
//! same connection carries the pairing mailbox.
//!
//! [`Home`] is that one connection. Sessions and pairings share it, because the
//! address a relay observes is the address a peer will be punched at, and a
//! second socket would be observed at a second address that nothing can reach.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tachyx_crypto::{Nameplate, RelayAddress};
use tachyx_relay::client::RelayClient;
use tachyx_relay::socket::{Routes, RoutingSocket};
use tachyx_relay::{Arrivals, RelayError};

use crate::control::RelayStatus;

/// How long the relay is given to report the address it sees this host at.
///
/// It arrives unprompted as soon as the connection is up, so this is a bound on
/// reaching the relay at all rather than on the relay thinking about it.
const OBSERVED: Duration = Duration::from_secs(5);

/// How long a mailbox lives on a relay.
///
/// The relay's own figure, from `spec/relay.md`. A pairing code offered for
/// longer than this would be a promise the host cannot keep: the mailbox behind
/// it is already closed.
pub const MAILBOX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(300);

/// What this host knows about the relays it is configured to use.
///
/// One row per configured relay, in the order they are tried. The presence task
/// writes them and `status` and `doctor` read them, so a relay that is not
/// reachable is reported as not reachable rather than probed again by every
/// command that asks.
#[derive(Debug)]
pub struct Presence {
    relays: Mutex<Vec<RelayStatus>>,
}

impl Presence {
    /// The configured relays, none of them reached yet.
    #[must_use]
    pub fn of(configured: &[RelayAddress]) -> Self {
        Self {
            relays: Mutex::new(
                configured
                    .iter()
                    .map(|relay| RelayStatus {
                        relay: relay.clone(),
                        connected: false,
                        rtt: None,
                    })
                    .collect(),
            ),
        }
    }

    /// What is known about each relay now.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RelayStatus> {
        guard(&self.relays).clone()
    }

    /// Whether any relay is reachable, which is whether anything off this
    /// network can find this host at all.
    #[must_use]
    pub fn any_connected(&self) -> bool {
        guard(&self.relays).iter().any(|relay| relay.connected)
    }

    /// Takes the configured list again, after the CLI has changed it.
    ///
    /// What is known about a relay that is still in the list is kept: the
    /// connection to it did not go away because a second relay was added beside
    /// it.
    pub fn reconfigure(&self, configured: &[RelayAddress]) {
        let mut relays = guard(&self.relays);
        let known = std::mem::take(&mut *relays);
        *relays = configured
            .iter()
            .map(|relay| {
                known
                    .iter()
                    .find(|held| held.relay == *relay)
                    .cloned()
                    .unwrap_or_else(|| RelayStatus {
                        relay: relay.clone(),
                        connected: false,
                        rtt: None,
                    })
            })
            .collect();
    }

    /// What the presence task measured on its connection to `relay`.
    ///
    /// A relay that is not in the configured list is ignored: the list is the
    /// configuration and this is a report about it.
    pub fn observed(&self, relay: &RelayAddress, connected: bool, rtt: Option<u32>) {
        if let Some(held) = guard(&self.relays)
            .iter_mut()
            .find(|held| held.relay == *relay)
        {
            held.connected = connected;
            held.rtt = rtt;
        }
    }
}

/// The one endpoint everything on this host leaves from, and the relay it is
/// registered with.
///
/// One socket, because `spec/relay.md` needs the address a relay observes to be
/// the address a peer is punched at. Two sockets would be observed at two
/// addresses and the punch would aim at the wrong one.
#[derive(Debug)]
pub struct Home {
    endpoint: quinn::Endpoint,
    socket: Arc<RoutingSocket>,
    /// Where a relayed peer's route is installed. The rendezvous writes it and
    /// the socket reads it on every packet.
    routes: Arc<Routes>,
    /// Told the moment a real session arrives, so a punch stops rather than
    /// running out its whole budget.
    arrivals: Arc<Arrivals>,
    /// `None` when no relay is configured. A host with no relay is reachable
    /// only from its own network and can pair with nobody.
    relay: Option<(RelayAddress, Arc<RelayClient>)>,
}

impl Home {
    /// Keeps `endpoint`, and the relay connection when there is one.
    #[must_use]
    pub const fn new(
        endpoint: quinn::Endpoint,
        socket: Arc<RoutingSocket>,
        routes: Arc<Routes>,
        arrivals: Arc<Arrivals>,
        relay: Option<(RelayAddress, Arc<RelayClient>)>,
    ) -> Self {
        Self {
            endpoint,
            socket,
            routes,
            arrivals,
            relay,
        }
    }

    /// The endpoint sessions are accepted on.
    #[must_use]
    pub const fn endpoint(&self) -> &quinn::Endpoint {
        &self.endpoint
    }

    /// Where a relayed peer's route is installed.
    #[must_use]
    pub const fn routes(&self) -> &Arc<Routes> {
        &self.routes
    }

    /// Told when a real session arrives.
    #[must_use]
    pub const fn arrivals(&self) -> &Arc<Arrivals> {
        &self.arrivals
    }

    /// The relay this host is registered with, if any.
    #[must_use]
    pub fn relay(&self) -> Option<&Arc<RelayClient>> {
        self.relay.as_ref().map(|(_, client)| client)
    }

    /// Where that relay is, as it is written in the configuration.
    #[must_use]
    pub fn address(&self) -> Option<&RelayAddress> {
        self.relay.as_ref().map(|(address, _)| address)
    }

    /// The socket underneath, which the relay client sends through.
    #[must_use]
    pub const fn socket(&self) -> &Arc<RoutingSocket> {
        &self.socket
    }

    /// Addresses a client should try before it asks a relay.
    ///
    /// The address the relay observed, and the bound address when it names a
    /// particular interface. A host bound to `0.0.0.0` offers only the observed
    /// one: enumerating every local interface needs a platform call this daemon
    /// does not make, so a client on the same network reaches this host through
    /// its relay-observed address rather than through a link-local one it was
    /// never told about.
    #[must_use]
    pub fn endpoints(&self, observed: Option<SocketAddr>) -> Vec<SocketAddr> {
        let mut found = Vec::new();
        if let Some(observed) = observed {
            found.push(observed);
        }
        if let Ok(local) = self.endpoint.local_addr()
            && !local.ip().is_unspecified()
            && !found.contains(&local)
        {
            found.push(local);
        }
        found
    }
}

/// A pairing mailbox on a relay.
///
/// Numbered, one-shot, two-party, and forwarded without being read. The relay
/// allocates the number, which is why the code cannot be shown before the
/// mailbox is open: the number is half of what the key exchange is keyed on.
#[derive(Debug)]
pub struct Mailbox {
    /// Which relay this mailbox is on. It goes into the pairing link so that
    /// the client knows where to join.
    relay: RelayAddress,
    /// The number the relay allocated.
    nameplate: Nameplate,
    /// This host's own addresses, worth trying before a relay is asked.
    endpoints: Vec<SocketAddr>,
    /// The home connection. Shared, not opened for this pairing: see [`Home`].
    client: Arc<RelayClient>,
}

impl Mailbox {
    /// Opens a mailbox on the relay `home` is registered with.
    ///
    /// # Errors
    /// If no relay is configured, or the relay cannot be reached or refuses.
    pub async fn open(home: &Home) -> Result<Self, MailboxError> {
        let (relay, client) = home
            .relay
            .as_ref()
            .ok_or_else(|| MailboxError::Unreachable("no relay is configured".to_owned()))?;
        // The relay reports this before anything asks, so waiting on it is
        // really waiting for the connection.
        let observed = client.wait_observed(OBSERVED).await.map_err(said)?;
        let nameplate = client.allocate().await.map_err(said)?;
        Ok(Self {
            relay: relay.clone(),
            nameplate,
            endpoints: home.endpoints(Some(observed)),
            client: Arc::clone(client),
        })
    }

    /// Which relay this mailbox is on.
    #[must_use]
    pub const fn relay(&self) -> &RelayAddress {
        &self.relay
    }

    /// The number the relay allocated.
    #[must_use]
    pub const fn nameplate(&self) -> Nameplate {
        self.nameplate
    }

    /// Addresses a client should try before it asks a relay.
    #[must_use]
    pub fn endpoints(&self) -> &[SocketAddr] {
        &self.endpoints
    }

    /// Sends one pairing flight to whoever else is in the mailbox.
    ///
    /// # Errors
    /// If the relay connection has gone.
    pub fn send(&mut self, flight: Vec<u8>) -> Result<(), MailboxError> {
        self.client.mail(flight).map_err(said)
    }

    /// Waits for the other party's flight.
    ///
    /// The caller bounds the wait: a code is on offer for as long as its time to
    /// live and no longer.
    ///
    /// # Errors
    /// If the relay connection has gone, or it refuses.
    pub async fn recv(&mut self, patience: Duration) -> Result<Vec<u8>, MailboxError> {
        self.client.next_mail(patience).await.map_err(said)
    }

    /// Closes the mailbox now.
    ///
    /// Taken by value: a mailbox is one attempt, and there is no second release
    /// to make. The connection stays, because it is the home connection and
    /// presence runs on it.
    ///
    /// A failure is not reported. The relay closes a mailbox on its own after
    /// five minutes, so the worst a lost `Release` costs is a nameplate held
    /// until then.
    pub fn release(self) {
        let _ = self.client.release();
    }
}

/// What the relay client said, in the words a person can act on.
fn said(error: RelayError) -> MailboxError {
    match error {
        RelayError::Refused(what) => MailboxError::Refused(what.to_string()),
        // Nothing arrived inside the wait. On the mailbox that is nobody using
        // the code, which is a different sentence from a relay that went away.
        RelayError::Silent => MailboxError::Silent,
        other => MailboxError::Unreachable(other.to_string()),
    }
}

/// Why a mailbox could not be used.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    /// The relay could not be reached, or presented another key.
    #[error("cannot reach the relay: {0}")]
    Unreachable(String),
    /// The relay refused, and said why.
    #[error("the relay refused: {0}")]
    Refused(String),
    /// Nothing arrived inside the wait.
    #[error("nobody used the code before it expired")]
    Silent,
}

/// The list, whether or not a thread died holding it.
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tachyx_crypto::DeviceKey;

    fn relay(seed: u8, address: &str) -> RelayAddress {
        RelayAddress {
            address: address.to_owned(),
            key: Some(DeviceKey::from_seed(&[seed; 32]).expect("a key").id()),
        }
    }

    #[test]
    fn a_relay_nothing_has_reached_is_reported_as_not_reached() {
        let configured = [
            relay(1, "100.64.0.2:47990"),
            relay(2, "relay.example:47990"),
        ];
        let presence = Presence::of(&configured);
        let snapshot = presence.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().all(|held| !held.connected));
        assert!(!presence.any_connected());
    }

    #[test]
    fn a_relay_added_beside_a_live_one_does_not_disconnect_it() {
        let held = relay(1, "100.64.0.2:47990");
        let added = relay(2, "relay.example:47990");
        let presence = Presence::of(std::slice::from_ref(&held));
        presence.observed(&held, true, Some(9));
        presence.reconfigure(&[held, added]);
        let snapshot = presence.snapshot();
        assert!(snapshot[0].connected, "the one that was up is still up");
        assert_eq!(snapshot[0].rtt, Some(9));
        assert!(!snapshot[1].connected, "and the new one has not been tried");
    }

    #[test]
    fn what_the_presence_task_measured_is_what_status_reports() {
        let configured = [relay(1, "100.64.0.2:47990")];
        let presence = Presence::of(&configured);
        presence.observed(&configured[0], true, Some(12));
        presence.observed(&relay(3, "somebody.else:47990"), true, Some(1));
        let snapshot = presence.snapshot();
        assert_eq!(snapshot.len(), 1, "a relay nobody configured is not a row");
        assert!(snapshot[0].connected);
        assert_eq!(snapshot[0].rtt, Some(12));
        assert!(presence.any_connected());
    }
}
