//! Who is here, which mailboxes are open, and which lanes belong to whom.
//!
//! Nothing here is stored. A relay that is restarted has forgotten everything,
//! and every host reconnects. That is deliberate: a relay holds no session key
//! and no account, so there is nothing worth keeping and nothing worth stealing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tachyx_crypto::{DeviceId, Nameplate};
use tachyx_relay::proto::{LaneToken, Refusal, RelayMessage};
use tokio::sync::mpsc;

/// How long a pairing mailbox lives.
///
/// Five minutes is longer than anybody takes to retype eight digits and short
/// enough that a mailbox nobody joined is gone before it is a resource.
pub const MAILBOX_LIFE: Duration = Duration::from_secs(300);

/// How many times a nameplate is drawn again after it collides.
///
/// A relay holding a handful of mailboxes collides about never. This bounds the
/// loop rather than describing what it does.
const DRAWS: usize = 64;

/// Where a message to one connection goes.
pub type Outbound = mpsc::UnboundedSender<RelayMessage>;

/// What the relay knows about a connection that registered.
#[derive(Debug, Clone)]
pub struct Registered {
    /// Which connection, so a stale one cannot unregister a newer one.
    pub id: u64,
    /// Where that peer's control connection is now, which is also where its
    /// spliced traffic goes.
    pub at: SocketAddr,
    /// Its control stream.
    pub out: Outbound,
}

/// One party of a mailbox.
#[derive(Debug, Clone)]
struct Party {
    id: u64,
    out: Outbound,
}

/// A numbered, one-shot, two-party message queue the relay forwards without
/// reading.
#[derive(Debug)]
struct Mailbox {
    opened: Instant,
    opener: Party,
    joiner: Option<Party>,
    /// The one flight the opener wrote before anybody joined.
    ///
    /// Only this direction needs holding. The opener is connected for as long as
    /// the mailbox exists, because the mailbox dies with its connection, so a
    /// message toward the opener is always delivered rather than stored.
    for_joiner: Option<Vec<u8>>,
}

/// Everything the relay holds, which is nothing that survives a restart.
#[derive(Debug, Default)]
pub struct Registry {
    next: AtomicU64,
    /// Every device with a control connection open, registered or not.
    ///
    /// An answer travels back along a conversation that is already open, so it
    /// is routed by this. Presence is the other map: a client that only wants to
    /// reach somebody does not register, and nothing can be routed **to** it.
    connected: Mutex<HashMap<DeviceId, Registered>>,
    peers: Mutex<HashMap<DeviceId, Registered>>,
    mailboxes: Mutex<HashMap<Nameplate, Mailbox>>,
    /// Which mailbox a connection is in, so a message does not scan them all.
    inside: Mutex<HashMap<u64, Nameplate>>,
    /// Which lane ends a connection owns, so they die with it.
    lanes: Mutex<HashMap<u64, Vec<LaneToken>>>,
}

impl Registry {
    /// A number no live connection has.
    pub fn number(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Notes that `device` has a control connection open.
    ///
    /// This is not presence. It is what lets an answer travel back to whoever
    /// asked, which the asker's own `Register` would otherwise be needed for and
    /// which a client does not send.
    pub fn connect(&self, device: DeviceId, entry: Registered) {
        guard(&self.connected).insert(device, entry);
    }

    /// Makes `device` reachable at this connection, replacing any earlier one.
    ///
    /// A host that reconnects before the relay noticed the old connection is
    /// gone would otherwise be unreachable at the connection that works.
    pub fn register(&self, device: DeviceId, entry: Registered) {
        guard(&self.peers).insert(device, entry);
    }

    /// Follows a peer whose address moved.
    ///
    /// Returns the lane ends that have to move with it.
    pub fn moved(&self, device: DeviceId, id: u64, at: SocketAddr) -> Vec<LaneToken> {
        for map in [&self.connected, &self.peers] {
            if let Some(entry) = guard(map).get_mut(&device)
                && entry.id == id
            {
                entry.at = at;
            }
        }
        guard(&self.lanes).get(&id).cloned().unwrap_or_default()
    }

    /// Where `device` is, if it is registered here.
    #[must_use]
    pub fn lookup(&self, device: DeviceId) -> Option<Registered> {
        guard(&self.peers).get(&device).cloned()
    }

    /// Sends one message to a device that has registered.
    ///
    /// # Errors
    /// If no connection is registered under that identifier: it is offline, or
    /// it has never been here.
    pub fn send_to(&self, device: DeviceId, message: RelayMessage) -> Result<(), Refusal> {
        Self::hand(&self.peers, device, message)
    }

    /// Sends one message back to a device that is merely connected.
    ///
    /// # Errors
    /// If that device's connection has gone since it asked.
    pub fn reply_to(&self, device: DeviceId, message: RelayMessage) -> Result<(), Refusal> {
        Self::hand(&self.connected, device, message)
    }

    /// One send, without the map's lock held across it.
    fn hand(
        map: &Mutex<HashMap<DeviceId, Registered>>,
        device: DeviceId,
        message: RelayMessage,
    ) -> Result<(), Refusal> {
        let out = guard(map)
            .get(&device)
            .map(|entry| entry.out.clone())
            .ok_or(Refusal::UnknownPeer)?;
        out.send(message).map_err(|_| Refusal::UnknownPeer)
    }

    /// Notes that `id` owns this end of a lane.
    pub fn attach_lane(&self, id: u64, token: LaneToken) {
        guard(&self.lanes).entry(id).or_default().push(token);
    }

    /// Opens a mailbox for this connection.
    ///
    /// Whatever mailbox this connection was in is closed first. A host that
    /// shows a code, is cancelled, and shows another is asking for a second
    /// pairing, not making a mistake, and the first code is spent either way.
    ///
    /// # Errors
    /// If the relay has no free nameplate, which on a relay of this size is an
    /// attack rather than a busy afternoon.
    pub fn allocate(&self, id: u64, out: Outbound) -> Result<Nameplate, Refusal> {
        let mut mailboxes = guard(&self.mailboxes);
        let mut inside = guard(&self.inside);
        close(&mut mailboxes, &mut inside, id);
        // Drawn at random from the free set rather than counted up, so the next
        // pairing's number cannot be guessed from this one's.
        let nameplate = std::iter::repeat_with(random_nameplate)
            .take(DRAWS)
            .find(|drawn| !mailboxes.contains_key(drawn))
            .ok_or(Refusal::NoNameplates)?;
        mailboxes.insert(
            nameplate,
            Mailbox {
                opened: Instant::now(),
                opener: Party { id, out },
                joiner: None,
                for_joiner: None,
            },
        );
        inside.insert(id, nameplate);
        Ok(nameplate)
    }

    /// Enters an open mailbox as its second party.
    ///
    /// Any flight the opener already wrote is handed over at once, so a client
    /// that joins late is not waiting on a message that has already been sent.
    ///
    /// Whatever mailbox this connection was in is closed first, for the same
    /// reason [`Registry::allocate`] closes one.
    ///
    /// # Errors
    /// If there is no such mailbox, or it already has two parties.
    pub fn join(&self, nameplate: Nameplate, id: u64, out: Outbound) -> Result<(), Refusal> {
        let mut mailboxes = guard(&self.mailboxes);
        let mut inside = guard(&self.inside);
        close(&mut mailboxes, &mut inside, id);
        let mailbox = mailboxes
            .get_mut(&nameplate)
            .ok_or(Refusal::UnknownNameplate)?;
        if mailbox.joiner.is_some() {
            return Err(Refusal::NameplateTaken);
        }
        if let Some(body) = mailbox.for_joiner.take() {
            out.send(RelayMessage::Mail { body })
                .map_err(|_| Refusal::UnknownNameplate)?;
        }
        mailbox.joiner = Some(Party { id, out });
        inside.insert(id, nameplate);
        Ok(())
    }

    /// Carries one flight to the other party.
    ///
    /// # Errors
    /// If this connection is in no mailbox, or the other party has not joined
    /// and has a flight waiting already.
    pub fn mail(&self, id: u64, body: Vec<u8>) -> Result<(), Refusal> {
        let mut mailboxes = guard(&self.mailboxes);
        let inside = guard(&self.inside);
        let nameplate = inside.get(&id).ok_or(Refusal::NotInMailbox)?;
        let mailbox = mailboxes
            .get_mut(nameplate)
            .ok_or(Refusal::UnknownNameplate)?;
        let to = if mailbox.opener.id == id {
            mailbox.joiner.as_ref()
        } else {
            Some(&mailbox.opener)
        };
        match to {
            Some(party) => party
                .out
                .send(RelayMessage::Mail { body })
                .map_err(|_| Refusal::NotInMailbox),
            None if mailbox.for_joiner.is_some() => {
                // One message per direction. A second before the first has been
                // read is a peer that has lost the exchange, not a peer that is
                // ahead of it.
                Err(Refusal::RateLimited)
            }
            None => {
                mailbox.for_joiner = Some(body);
                Ok(())
            }
        }
    }

    /// Closes whatever mailbox this connection is in.
    ///
    /// Either party closes it, because a pairing that ended has no second act
    /// and one attempt per code is the rule the host holds either way.
    pub fn release(&self, id: u64) {
        // `mailboxes` before `inside`, everywhere, or two of these deadlock.
        let mut mailboxes = guard(&self.mailboxes);
        let mut inside = guard(&self.inside);
        close(&mut mailboxes, &mut inside, id);
    }

    /// Drops everything a connection held.
    ///
    /// Returns the lane ends to close, because a lane dies with either control
    /// connection.
    pub fn forget(&self, device: DeviceId, id: u64) -> Vec<LaneToken> {
        for map in [&self.connected, &self.peers] {
            // Only if it is still this connection's entry: a device that
            // reconnected already replaced it, and a stale connection noticing
            // it is gone must not take the working one with it.
            let mut held = guard(map);
            if held.get(&device).is_some_and(|entry| entry.id == id) {
                held.remove(&device);
            }
        }
        self.release(id);
        guard(&self.lanes).remove(&id).unwrap_or_default()
    }

    /// Closes every mailbox older than [`MAILBOX_LIFE`].
    ///
    /// Returns how many were closed, which is what a log line reports.
    pub fn expire(&self, now: Instant) -> usize {
        let mut mailboxes = guard(&self.mailboxes);
        let mut inside = guard(&self.inside);
        let stale: Vec<Nameplate> = mailboxes
            .iter()
            .filter(|(_, mailbox)| now.duration_since(mailbox.opened) > MAILBOX_LIFE)
            .map(|(nameplate, _)| *nameplate)
            .collect();
        for nameplate in &stale {
            if let Some(mailbox) = mailboxes.remove(nameplate) {
                inside.remove(&mailbox.opener.id);
                if let Some(joiner) = mailbox.joiner {
                    inside.remove(&joiner.id);
                }
            }
        }
        stale.len()
    }

    /// How many hosts are reachable through this relay.
    #[must_use]
    pub fn registered(&self) -> usize {
        guard(&self.peers).len()
    }

    /// How many pairings are in progress.
    #[must_use]
    pub fn pairing(&self) -> usize {
        guard(&self.mailboxes).len()
    }
}

/// Closes whatever mailbox `id` is in, with both tables already held.
fn close(
    mailboxes: &mut HashMap<Nameplate, Mailbox>,
    inside: &mut HashMap<u64, Nameplate>,
    id: u64,
) {
    let Some(nameplate) = inside.remove(&id) else {
        return;
    };
    if let Some(mailbox) = mailboxes.remove(&nameplate) {
        inside.remove(&mailbox.opener.id);
        if let Some(joiner) = mailbox.joiner {
            inside.remove(&joiner.id);
        }
    }
}

/// A mailbox number, drawn uniformly and never zero.
fn random_nameplate() -> Nameplate {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0u8; 2];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system random source is readable");
    Nameplate::new(u16::from_le_bytes(bytes)).unwrap_or_else(|| {
        Nameplate::new(1).expect("one is not zero")
    })
}

/// The table, whether or not a thread died holding it.
fn guard<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(seed: u8) -> DeviceId {
        tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
            .expect("a key")
            .id()
    }

    fn channel() -> (Outbound, mpsc::UnboundedReceiver<RelayMessage>) {
        mpsc::unbounded_channel()
    }

    fn at(last: u8) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, last], 47_990))
    }

    #[test]
    fn a_host_that_reconnects_replaces_the_connection_that_is_gone() {
        let registry = Registry::default();
        let (first, _first_in) = channel();
        let (second, mut second_in) = channel();
        let one = registry.number();
        let two = registry.number();
        registry.register(
            device(1),
            Registered {
                id: one,
                at: at(1),
                out: first,
            },
        );
        registry.register(
            device(1),
            Registered {
                id: two,
                at: at(2),
                out: second,
            },
        );
        registry
            .send_to(device(1), RelayMessage::Observed { address: at(2) })
            .expect("the newer connection is the one that is reachable");
        assert!(second_in.try_recv().is_ok());

        // The old connection noticing it is gone must not take the new one with
        // it.
        assert!(registry.forget(device(1), one).is_empty());
        assert!(registry.lookup(device(1)).is_some());
    }

    #[test]
    fn a_pairing_flight_written_before_anybody_joined_is_waiting_when_they_do() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let (client, mut client_in) = channel();
        let opener = registry.number();
        let joiner = registry.number();
        let nameplate = registry.allocate(opener, host).expect("a mailbox");
        registry.mail(opener, b"flight one".to_vec()).expect("held");
        registry.join(nameplate, joiner, client).expect("joined");
        assert!(matches!(
            client_in.try_recv(),
            Ok(RelayMessage::Mail { body }) if body == b"flight one"
        ));
    }

    #[test]
    fn a_second_flight_before_the_first_is_read_is_refused() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let opener = registry.number();
        registry.allocate(opener, host).expect("a mailbox");
        registry.mail(opener, b"one".to_vec()).expect("held");
        assert_eq!(
            registry.mail(opener, b"two".to_vec()),
            Err(Refusal::RateLimited)
        );
    }

    #[test]
    fn a_host_that_shows_a_second_code_closes_the_first_mailbox() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let opener = registry.number();
        let first = registry.allocate(opener, host.clone()).expect("a mailbox");
        let second = registry.allocate(opener, host).expect("a second mailbox");
        assert_ne!(first, second);
        assert_eq!(registry.pairing(), 1, "the first code is spent, not held");
    }

    #[test]
    fn a_mailbox_takes_two_parties_and_no_more() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let (client, _client_in) = channel();
        let (third, _third_in) = channel();
        let opener = registry.number();
        let nameplate = registry.allocate(opener, host).expect("a mailbox");
        registry
            .join(nameplate, registry.number(), client)
            .expect("joined");
        assert_eq!(
            registry.join(nameplate, registry.number(), third),
            Err(Refusal::NameplateTaken)
        );
    }

    #[test]
    fn a_nameplate_that_names_no_mailbox_is_refused() {
        let registry = Registry::default();
        let (client, _client_in) = channel();
        assert_eq!(
            registry.join(
                Nameplate::new(4242).expect("a nameplate"),
                registry.number(),
                client
            ),
            Err(Refusal::UnknownNameplate)
        );
    }

    #[test]
    fn a_flight_from_a_connection_in_no_mailbox_is_refused() {
        let registry = Registry::default();
        assert_eq!(
            registry.mail(registry.number(), b"nowhere".to_vec()),
            Err(Refusal::NotInMailbox)
        );
    }

    #[test]
    fn closing_a_mailbox_takes_both_parties_out_of_it() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let (client, _client_in) = channel();
        let opener = registry.number();
        let joiner = registry.number();
        let nameplate = registry.allocate(opener, host).expect("a mailbox");
        registry.join(nameplate, joiner, client).expect("joined");
        registry.release(joiner);
        assert_eq!(registry.pairing(), 0);
        assert_eq!(
            registry.mail(opener, b"anything".to_vec()),
            Err(Refusal::NotInMailbox)
        );
    }

    #[test]
    fn a_joiner_that_leaves_takes_the_mailbox_with_it() {
        // The structural half of *one attempt per code* on an honest relay:
        // the first join owns the mailbox for good. A joiner that walks away
        // does not free the slot for a second guess, it closes the mailbox,
        // and the opener's next code is a new mailbox with a new secret.
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let opener = registry.number();
        let nameplate = registry.allocate(opener, host).expect("a mailbox");

        let (client, _client_in) = channel();
        let joiner = registry.number();
        registry.join(nameplate, joiner, client).expect("joined");
        registry.release(joiner);

        let (again, _again_in) = channel();
        let second = registry.number();
        assert_eq!(
            registry.join(nameplate, second, again),
            Err(Refusal::UnknownNameplate),
            "the mailbox died with its first joiner rather than freeing the slot"
        );
    }

    #[test]
    fn a_mailbox_nobody_joined_is_gone_five_minutes_later() {
        let registry = Registry::default();
        let (host, _host_in) = channel();
        let opener = registry.number();
        registry.allocate(opener, host).expect("a mailbox");
        assert_eq!(registry.expire(Instant::now()), 0);
        assert_eq!(
            registry.expire(Instant::now() + MAILBOX_LIFE + Duration::from_secs(1)),
            1
        );
        assert_eq!(registry.pairing(), 0);
    }

    #[test]
    fn a_host_that_is_not_here_is_named_as_not_here() {
        let registry = Registry::default();
        assert_eq!(
            registry.send_to(device(9), RelayMessage::Observed { address: at(1) }),
            Err(Refusal::UnknownPeer)
        );
    }
}
