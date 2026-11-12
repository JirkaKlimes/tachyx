//! The sessions running now, so that `status` can list them and
//! `sessions kill` can end one.
//!
//! A row exists for exactly as long as the task serving that session: the
//! [`Handle`] the session holds takes the row out of the list when it is
//! dropped, however the session ended. Nothing has to remember to tidy up.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tachyx_crypto::DeviceId;

use crate::control::SessionStatus;

/// What `status` lists a session as, and what `sessions kill` names it by.
///
/// A counter from one, drawn at every start. It means nothing after the daemon
/// restarts, which is the honest lifetime of a thing that only exists while a
/// connection does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u32);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.trim().parse().map(Self)
    }
}

/// Who is on the other end of a session, as the handshake and the relay knew it
/// before the session itself started.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Which device. The handshake compared this against the paired list.
    pub device: DeviceId,
    /// What that device is called in the paired list.
    pub name: String,
    /// Whether its packets come through a relay rather than straight off the
    /// wire.
    pub relayed: bool,
}

/// The numbers a session publishes about itself while it runs.
///
/// Written once a heartbeat, which is often enough for a person watching
/// `status` and rare enough that the session pays nothing for it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Live {
    /// The width being encoded.
    pub width: u32,
    /// The height being encoded.
    pub height: u32,
    /// The frame rate, in thousandths of a hertz.
    pub millihertz: u32,
    /// What the encoder is being asked for, in bits a second.
    pub bitrate: u32,
    /// The round trip the transport measures, in milliseconds.
    pub rtt: u32,
    /// Median capture to wire time over the last heartbeat interval, in
    /// microseconds.
    pub pipeline_micros: u32,
    /// The share of video packets the client last reported missing.
    pub loss: f64,
    /// Bytes sent on the connection.
    pub sent: u64,
    /// Bytes received on it.
    pub received: u64,
}

/// One live session's row.
#[derive(Debug)]
struct Entry {
    id: SessionId,
    peer: Peer,
    address: String,
    codec: String,
    opened: Instant,
    connection: quinn::Connection,
    live: Arc<Mutex<Live>>,
}

/// Every session running now.
#[derive(Debug, Default)]
pub struct Sessions {
    open: Mutex<Vec<Entry>>,
    next: AtomicU32,
}

impl Sessions {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts a session in the list, and hands back the handle that keeps it
    /// there.
    #[must_use]
    pub fn open(
        self: &Arc<Self>,
        peer: Peer,
        codec: String,
        connection: quinn::Connection,
    ) -> Handle {
        // Counted from one, because a session nobody can name `sessions kill 0`
        // for is a session nobody can end. The `+ 1` panics on the four
        // billionth session of one daemon, which is four billion live
        // connections into a process that has not been restarted.
        let id = SessionId(self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let live = Arc::new(Mutex::new(Live::default()));
        guard(&self.open).push(Entry {
            id,
            peer,
            address: connection.remote_address().to_string(),
            codec,
            opened: Instant::now(),
            connection,
            live: Arc::clone(&live),
        });
        Handle {
            sessions: Arc::clone(self),
            id,
            live,
        }
    }

    /// Every session running now, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<SessionStatus> {
        let now = Instant::now();
        guard(&self.open)
            .iter()
            .map(|entry| {
                let live = *guard(&entry.live);
                SessionStatus {
                    id: entry.id,
                    device: entry.peer.device,
                    name: entry.peer.name.clone(),
                    address: entry.address.clone(),
                    relayed: entry.peer.relayed,
                    codec: entry.codec.clone(),
                    width: live.width,
                    height: live.height,
                    millihertz: live.millihertz,
                    bitrate: live.bitrate,
                    rtt: live.rtt,
                    pipeline_micros: live.pipeline_micros,
                    loss: live.loss,
                    uptime: now.duration_since(entry.opened).as_secs(),
                    sent: live.sent,
                    received: live.received,
                }
            })
            .collect()
    }

    /// How many sessions are running.
    #[must_use]
    pub fn count(&self) -> usize {
        guard(&self.open).len()
    }

    /// Ends the session `id`, reporting whether there was one.
    ///
    /// The connection is closed rather than the task cancelled: the session
    /// loop ends by itself when its control stream does, and it then tears the
    /// pipeline down the same way it does for a client that walked away. One
    /// path out of a session is enough.
    pub fn kill(&self, id: SessionId) -> bool {
        self.close_where(|entry| entry.id == id, "closed from the host") > 0
    }

    /// Ends every session `device` has open, and reports how many there were.
    ///
    /// A session reads the device's permissions once, when it opens. A change
    /// to the pairing therefore takes effect when the sessions opened under the
    /// old one end.
    pub fn end_for(&self, device: DeviceId) -> usize {
        self.close_where(
            |entry| entry.peer.device == device,
            "the host changed this device's pairing",
        )
    }

    /// Closes the connection of every row `which` selects. Each session then
    /// ends by itself, as [`Sessions::kill`] describes.
    fn close_where(&self, which: impl Fn(&Entry) -> bool, reason: &str) -> usize {
        let mut closed = 0;
        for entry in guard(&self.open).iter().filter(|entry| which(entry)) {
            entry
                .connection
                .close(quinn::VarInt::from_u32(0), reason.as_bytes());
            closed += 1;
        }
        closed
    }

    /// Takes a row out, which is what dropping a [`Handle`] does.
    fn close(&self, id: SessionId) {
        guard(&self.open).retain(|entry| entry.id != id);
    }
}

/// A session's own row, held for as long as the session runs.
#[derive(Debug)]
pub struct Handle {
    sessions: Arc<Sessions>,
    id: SessionId,
    live: Arc<Mutex<Live>>,
}

impl Handle {
    /// What `status` will report about this session from now on.
    pub fn publish(&self, live: Live) {
        *guard(&self.live) = live;
    }

    /// Which session this is.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.sessions.close(self.id);
    }
}

/// The list, whether or not a thread died holding it.
///
/// A poisoned lock here means a session panicked while publishing a number for
/// a person to read. Refusing every later session over that would be the larger
/// fault.
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_identifier_survives_a_round_trip_through_what_status_prints() {
        assert_eq!("7".parse::<SessionId>().unwrap(), SessionId(7));
        assert_eq!(SessionId(7).to_string(), "7");
        assert!("not a session".parse::<SessionId>().is_err());
    }

    #[test]
    fn killing_a_session_that_is_not_there_says_so() {
        let sessions = Arc::new(Sessions::new());
        assert!(!sessions.kill(SessionId(1)));
        assert!(sessions.list().is_empty());
        assert_eq!(sessions.count(), 0);
    }
}
