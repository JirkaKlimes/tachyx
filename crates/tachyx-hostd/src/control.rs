//! The socket everything but `run` talks over.
//!
//! One Unix socket in the runtime directory, owned by the daemon. Every
//! subcommand of `tachyx-hostd` is a thin client over it: it opens the socket,
//! sends one [`Request`], reads the [`Response`] or responses that answer it,
//! and closes.
//!
//! The framing is the session's own, from
//! [`tachyx_transport::control`]: a `u32` big-endian length and that many
//! `postcard` bytes. There is no version negotiation, because both ends of this
//! socket are one binary and a daemon of one version cannot be talking to a CLI
//! of another.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tachyx_crypto::store::PairedClient;
use tachyx_crypto::{DeviceId, Permissions, RelayAddress};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use crate::sessions::SessionId;

/// The most bytes either end will read in one message.
///
/// The longest message here is a list of paired devices, at about a hundred
/// bytes a row. Anything approaching this is an end that has lost the framing,
/// and reading it would mean allocating on its say-so.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// Bytes of length prefix in front of each message.
const HEADER: usize = 4;

/// A device identifier as the base32 a person reads, rather than as the 32
/// bytes the session's wire carries.
///
/// Both ends of this socket are the same binary, so the few bytes it costs buy
/// nothing back, and `--json` prints these structures directly: an identifier
/// as an array of 32 numbers would be a worse thing for a script to read than
/// the text a person would have pasted.
mod as_text {
    use serde::{Deserialize as _, Deserializer, Serializer};
    use tachyx_crypto::DeviceId;

    pub(super) fn serialize<S: Serializer>(
        id: &DeviceId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_base32())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DeviceId, D::Error> {
        let text = String::deserialize(deserializer)?;
        DeviceId::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// What a subcommand asks the daemon to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// This host's device identifier.
    Id,
    /// What the daemon is doing.
    Status,
    /// Put a pairing code on offer, and report what happens to it.
    ///
    /// This is the one request answered by more than one response.
    Pair {
        /// What the device that uses the code will be granted.
        permissions: Permissions,
        /// How long the code stays on offer, in seconds.
        ttl: u32,
    },
    /// The devices this host has paired with.
    PairingsList,
    /// Forget a device, which is the whole of revocation.
    PairingsRevoke {
        /// Which device.
        #[serde(with = "as_text")]
        device: DeviceId,
    },
    /// Change what a device may do.
    PairingsSet {
        /// Which device.
        #[serde(with = "as_text")]
        device: DeviceId,
        /// What it may do from now on.
        permissions: Permissions,
    },
    /// Change the label a device is listed under.
    PairingsRename {
        /// Which device.
        #[serde(with = "as_text")]
        device: DeviceId,
        /// What to call it.
        name: String,
    },
    /// End a live session now.
    SessionsKill {
        /// Which session, as `status` lists it.
        id: SessionId,
    },
    /// The relays this host looks for.
    RelayList,
    /// Add a relay to the list, replacing any relay at the same address.
    RelayAdd {
        /// Where it is and which key it must present.
        relay: RelayAddress,
    },
    /// Remove every relay at an address.
    RelayRemove {
        /// The address, without a key.
        address: String,
    },
    /// Check everything that has to be true before a session can work.
    Doctor,
}

/// What the daemon answers with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// This host's device identifier.
    Id(#[serde(with = "as_text")] DeviceId),
    /// What the daemon is doing.
    Status(Status),
    /// One step of a pairing. Several of these arrive for one request.
    Pairing(PairingEvent),
    /// The devices this host has paired with.
    Pairings(Vec<Pairing>),
    /// The relays this host looks for, in the order they are tried.
    Relays(Vec<RelayAddress>),
    /// What `doctor` found.
    Doctor(Report),
    /// It was done, and there is nothing to report about it.
    Done,
    /// It was not done.
    Failed {
        /// A sentence for a person, which is also what `--json` prints.
        reason: String,
    },
}

/// What the daemon is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// This host's identifier.
    #[serde(with = "as_text")]
    pub device: DeviceId,
    /// What it calls itself in a pairing.
    pub name: String,
    /// The build.
    pub version: String,
    /// How long it has been running, in seconds.
    pub uptime: u64,
    /// Where it is listening for sessions.
    pub listen: String,
    /// The codecs its encoders were probed to offer.
    pub encoders: Vec<String>,
    /// How many devices are paired.
    pub paired: usize,
    /// The relays it looks for, and what it knows about each.
    pub relays: Vec<RelayStatus>,
    /// The sessions running now.
    pub sessions: Vec<SessionStatus>,
}

/// One configured relay, and whether this host has reached it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    /// Where it is and which key it must present.
    pub relay: RelayAddress,
    /// Whether the presence connection to it is up now.
    pub connected: bool,
    /// The round trip to it, in **microseconds**, when there is a connection to
    /// measure it on.
    ///
    /// Microseconds and not milliseconds: a relay on the same network is well
    /// under one millisecond away, and a figure that rounds it to zero says
    /// nothing about whether the path is healthy.
    pub rtt: Option<u32>,
}

/// One live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    /// What `sessions kill` names it by. It counts from one at every start and
    /// means nothing after the daemon restarts.
    pub id: SessionId,
    /// Which device is on the other end.
    #[serde(with = "as_text")]
    pub device: DeviceId,
    /// What that device is called in the pairing list.
    pub name: String,
    /// Where its packets come from.
    pub address: String,
    /// Whether they come through a relay rather than straight off the wire.
    pub relayed: bool,
    /// The codec the picture is in.
    pub codec: String,
    /// The width being encoded.
    pub width: u32,
    /// The height being encoded.
    pub height: u32,
    /// The frame rate, in thousandths of a hertz, which is the unit a refresh
    /// rate is honestly expressed in.
    pub millihertz: u32,
    /// What the encoder is being asked for, in bits a second.
    pub bitrate: u32,
    /// The round trip the transport measures, in milliseconds.
    pub rtt: u32,
    /// Median time from a frame leaving the compositor to its last datagram
    /// leaving this host, over the last heartbeat interval, in microseconds.
    ///
    /// The host's own half of the glass-to-glass figure, and the half a client
    /// cannot work out for itself. Zero on a still screen, where no picture went
    /// out to be timed.
    pub pipeline_micros: u32,
    /// The share of video packets the client reported missing, from zero to
    /// one.
    pub loss: f64,
    /// How long the session has been up, in seconds.
    pub uptime: u64,
    /// Bytes this host has sent on the connection.
    pub sent: u64,
    /// Bytes this host has received on it.
    pub received: u64,
}

/// One device this host has paired with.
///
/// The same rows [`PairedClient`] holds, written flat. The file writes the two
/// permissions as a flattened table, which `postcard` cannot read back: it has
/// no field names on the wire and `serde`'s flattening needs them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    /// Which device. This is what a handshake is compared against.
    #[serde(with = "as_text")]
    pub device: DeviceId,
    /// What it is called in the list. Chosen by the peer, so it names a row and
    /// decides nothing.
    pub name: String,
    /// Whether it may inject input, as against watching only.
    pub control: bool,
    /// Whether a session opens without somebody at this host accepting it.
    pub unattended: bool,
    /// When it was paired, in seconds since the Unix epoch.
    pub paired: u64,
}

impl Pairing {
    /// The row as the daemon holds it.
    #[must_use]
    pub fn of(client: &PairedClient) -> Self {
        Self {
            device: client.device,
            name: client.name.clone(),
            control: client.permissions.control,
            unattended: client.permissions.unattended,
            paired: client.paired,
        }
    }

    /// What this device may do.
    #[must_use]
    pub const fn permissions(&self) -> Permissions {
        Permissions {
            control: self.control,
            unattended: self.unattended,
        }
    }
}

/// One step of a pairing, as it happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingEvent {
    /// The code is on offer. Both forms of it: the digits somebody retypes and
    /// the link a camera reads.
    Waiting {
        /// The typed code, as it is shown: `7-4921-8306`.
        typed: String,
        /// The link the QR carries.
        uri: String,
    },
    /// Somebody answered, and the code is spent either way.
    ///
    /// The relay has no message for a peer entering a mailbox, so this is not
    /// that: it is sent when the client's own flight arrives, which is the
    /// first this host hears of anybody. Whether the flight opens is the next
    /// event, not this one. See `spec/relay.md`.
    Joined,
    /// A device is paired.
    Succeeded {
        /// Which device.
        #[serde(with = "as_text")]
        device: DeviceId,
        /// What it calls itself.
        name: String,
        /// The three symbols both ends show, for somebody who wants to check
        /// the code was not overheard.
        words: String,
    },
    /// It did not happen, and the code is burnt either way.
    Failed {
        /// A sentence for a person.
        reason: String,
    },
}

/// What `doctor` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// One row per thing that has to be true, in the order they matter.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether nothing failed. A warning is not a failure: a host with nothing
    /// paired works, it simply accepts nobody yet.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.state != State::Fail)
    }
}

/// One thing that has to be true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// A short name a script matches on, such as `encoder`. Stable.
    pub check: String,
    /// How it came out.
    pub state: State,
    /// A plain sentence saying what is true, or what is not and what to do.
    pub detail: String,
}

impl Check {
    /// A check that came out either way.
    #[must_use]
    pub fn new(check: &str, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            state: if ok { State::Pass } else { State::Fail },
            detail: detail.into(),
        }
    }

    /// A check that works today and will not tomorrow.
    #[must_use]
    pub fn warn(check: &str, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            state: State::Warn,
            detail: detail.into(),
        }
    }
}

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// True.
    Pass,
    /// True, and worth saying out loud anyway.
    Warn,
    /// Not true, and a session will not work until it is.
    Fail,
}

/// Where the control socket lives.
///
/// The runtime directory, which the login session owns and which is cleared
/// when it ends. `/run/tachyx` is the fallback for a daemon started by the
/// system rather than by a user, which has no runtime directory of its own, and
/// for a `XDG_RUNTIME_DIR` that is empty or relative, which names no directory.
#[must_use]
pub fn socket_path() -> PathBuf {
    crate::config::runtime_directory()
        .unwrap_or_else(|| PathBuf::from("/run"))
        .join("tachyx")
        .join("hostd.sock")
}

/// Opens the control socket at `path`.
///
/// **The directory is the boundary.** Whoever can enter it can ask this daemon
/// to pair a device or to revoke one, so it is created at mode 0700 by
/// `mkdir(2)` itself. The mode cannot be set afterwards: `bind(2)` and
/// `mkdir(2)` both make the node with `0777 & ~umask`, and under a permissive
/// umask everything between the call that creates it and the call that narrows
/// it is a window in which anybody may enter. A directory nobody else may enter
/// is what makes the socket's own mode not load-bearing, and the 0600 on the
/// socket below is defence in depth rather than the defence.
///
/// A symlink already at the directory's name is refused. `set_permissions`
/// follows one, so the alternative is chmodding whatever it points at.
///
/// A socket file left behind by a daemon that was killed is removed, but only
/// after a connection to it is refused. A socket somebody is listening on is a
/// second daemon, and taking it from them would leave both half working.
///
/// # Errors
/// If another daemon is listening, the directory is a symlink, or the directory
/// or the socket cannot be made.
pub async fn listen(path: &Path) -> Result<UnixListener, ControlError> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let failed = |source| ControlError::Socket {
        path: path.to_owned(),
        source,
    };
    if let Some(directory) = path.parent() {
        match std::fs::symlink_metadata(directory) {
            Ok(found) if found.is_symlink() => {
                return Err(ControlError::Symlinked(directory.to_owned()));
            }
            // A directory an earlier build left wider, which is not a symlink
            // because the line above refused that.
            Ok(found) if found.is_dir() && found.permissions().mode() & 0o777 != 0o700 => {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .map_err(failed)?;
            }
            _ => {}
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(directory)
            .map_err(failed)?;
    }
    match UnixStream::connect(path).await {
        Ok(_) => return Err(ControlError::Taken(path.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        // Refused, or a file that is not a socket at all. Either way nothing is
        // listening on it and it is this daemon's path to take.
        Err(_) => std::fs::remove_file(path).map_err(failed)?,
    }
    let listener = UnixListener::bind(path).map_err(failed)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(failed)?;
    Ok(listener)
}

/// Whether the far end of `stream` is this same user.
///
/// The 0700 directory around the socket says this once already, and this says
/// it again at the other end: a socket that outlived a change to the directory,
/// or a directory somebody widened, does not become an open door. A connection
/// that is not this user's runs `pairings set --allow control` otherwise.
///
/// Root passes. It can enter any directory and read any file on this machine,
/// so refusing it here would be a claim this daemon cannot back. A credential
/// that cannot be read at all does not pass.
#[must_use]
pub fn is_own_user(stream: &UnixStream) -> bool {
    let us = rustix::process::getuid().as_raw();
    stream
        .peer_cred()
        .is_ok_and(|peer| peer.uid() == us || peer.uid() == 0)
}

/// Opens a connection to the running daemon.
///
/// # Errors
/// [`ControlError::NotRunning`] when there is nothing at the other end, which
/// is the sentence a person needs rather than the name of a path they did not
/// choose.
pub async fn connect(path: &Path) -> Result<Wire, ControlError> {
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(Wire::new(stream)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(ControlError::NotRunning)
        }
        Err(source) => Err(ControlError::Socket {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Asks the daemon one question and reads the one answer.
///
/// # Errors
/// If the daemon is not running, or the connection fails.
pub async fn ask(path: &Path, request: &Request) -> Result<Response, ControlError> {
    let mut wire = connect(path).await?;
    wire.send(request).await?;
    wire.recv().await
}

/// One end of a control connection.
#[derive(Debug)]
pub struct Wire {
    stream: UnixStream,
    /// Bytes taken off the socket and not yet framed, including any head of the
    /// next message that arrived with this one.
    pending: Vec<u8>,
    /// The body of the message last framed, and the buffer a message is built
    /// in before it is sent.
    scratch: Vec<u8>,
}

impl Wire {
    /// Wraps a connected socket.
    #[must_use]
    pub const fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Writes one message.
    ///
    /// **Not cancel-safe.** The length and the body go out in one `write_all`,
    /// which is a loop over `poll_write`: a future dropped part way through
    /// leaves however many bytes the kernel took, and the far end then reads
    /// the next message out of the middle of this one and never frames again.
    /// Nothing selects on this today. Anything that starts to has to make the
    /// message whole before it may be cancelled, and a buffer of its own is
    /// what that costs.
    ///
    /// # Errors
    /// If the message will not encode, is longer than [`MAX_MESSAGE`], or the
    /// far end has gone.
    pub async fn send<M: Serialize>(&mut self, message: &M) -> Result<(), ControlError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0u8; HEADER]);
        postcard::to_io(message, &mut self.scratch).map_err(|_| ControlError::Unencodable)?;
        let len = self.scratch.len() - HEADER;
        let header = u32::try_from(len)
            .ok()
            .filter(|_| len <= MAX_MESSAGE)
            .ok_or(ControlError::TooLong { len })?;
        self.scratch[..HEADER].copy_from_slice(&header.to_be_bytes());
        self.stream
            .write_all(&self.scratch)
            .await
            .map_err(ControlError::Io)
    }

    /// Reads one message.
    ///
    /// Cancel-safe. A message is a length and then the body it announces, and a
    /// read cancelled between the two would take the bytes it had already
    /// consumed with it: the next length would be read out of the middle of a
    /// message and the framing would never square again. So bytes come off the
    /// socket into a buffer that outlives any one call, framing is done over
    /// that buffer, and the one await is `read_buf`, which reports bytes only
    /// once it has taken them.
    ///
    /// # Errors
    /// If the connection ends, the far end announces a message longer than
    /// [`MAX_MESSAGE`], or the bytes are not the message expected.
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<M, ControlError> {
        loop {
            if let Some(len) = self.announced()?
                && self.pending.len() >= HEADER + len
            {
                self.scratch.clear();
                self.scratch
                    .extend_from_slice(&self.pending[HEADER..HEADER + len]);
                self.pending.drain(..HEADER + len);
                return postcard::from_bytes(&self.scratch).map_err(|_| ControlError::Undecodable);
            }
            if self
                .stream
                .read_buf(&mut self.pending)
                .await
                .map_err(ControlError::Io)?
                == 0
            {
                return Err(ControlError::Ended);
            }
        }
    }

    /// The body length the buffered header announces, once there is one.
    fn announced(&self) -> Result<Option<usize>, ControlError> {
        let Some(header) = self.pending.get(..HEADER) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(header.try_into().expect("four bytes")) as usize;
        if len > MAX_MESSAGE {
            return Err(ControlError::TooLong { len });
        }
        Ok(Some(len))
    }
}

/// Why the control socket could not carry a message.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// Nothing is listening. This is the sentence the CLI prints: a person who
    /// ran `tachyx-hostd status` needs to know the daemon is not running, not
    /// which path was refused.
    #[error("the daemon is not running")]
    NotRunning,
    /// Something else is already listening there.
    #[error("another tachyx-hostd is already listening on {0}")]
    Taken(PathBuf),
    /// The control directory is a symlink, so making it 0700 would make
    /// whatever it points at 0700 instead.
    #[error("{0} is a symlink: the control directory has to be a directory this daemon made")]
    Symlinked(PathBuf),
    /// The socket or its directory could not be reached.
    #[error("cannot use the control socket at {path}")]
    Socket {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The connection failed part way.
    #[error("the control connection failed")]
    Io(#[source] std::io::Error),
    /// The far end closed, between two messages or part way through one.
    #[error("the control connection ended")]
    Ended,
    /// The far end opened a connection and did not finish a request on it. One
    /// of those holds a reader and its buffer for as long as it is left there.
    #[error("the control connection went quiet part way through a request")]
    TimedOut,
    /// A message that will not fit the framing.
    #[error("a control message of {len} bytes exceeds the {MAX_MESSAGE}-byte limit")]
    TooLong {
        /// The length announced or produced.
        len: usize,
    },
    /// A message that will not encode.
    #[error("a control message could not be encoded")]
    Unencodable,
    /// Bytes that are not the message expected.
    #[error("a control message could not be decoded")]
    Undecodable,
    /// The daemon answered something this request has no use for, which is a
    /// fault in this build rather than in anything a person typed.
    #[error("the daemon answered something else")]
    Unexpected,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(seed: u8) -> DeviceId {
        tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
            .expect("a key")
            .id()
    }

    fn pairing(seed: u8) -> Pairing {
        Pairing {
            device: device(seed),
            name: "phone".to_owned(),
            control: true,
            unattended: false,
            paired: 1_700_000_000,
        }
    }

    #[test]
    fn every_response_survives_a_round_trip_through_the_framing() {
        let responses = [
            Response::Id(device(1)),
            Response::Pairings(vec![pairing(1), pairing(2)]),
            Response::Relays(vec![RelayAddress {
                address: "100.64.0.2:47990".to_owned(),
                key: Some(device(3)),
            }]),
            Response::Pairing(PairingEvent::Waiting {
                typed: "7-4921-8306".to_owned(),
                uri: "tachyx://pair?c=AAAA".to_owned(),
            }),
            Response::Doctor(Report {
                checks: vec![Check::new("encoder", true, "an encoder was found")],
            }),
            Response::Done,
            Response::Failed {
                reason: "no such device".to_owned(),
            },
        ];
        for response in responses {
            let bytes = postcard::to_stdvec(&response).expect("a response encodes");
            assert_eq!(
                postcard::from_bytes::<Response>(&bytes).expect("and decodes"),
                response
            );
        }
    }

    #[test]
    fn every_request_survives_a_round_trip_through_the_framing() {
        let requests = [
            Request::Status,
            Request::Pair {
                permissions: Permissions::default(),
                ttl: 300,
            },
            Request::PairingsRevoke { device: device(4) },
            Request::PairingsSet {
                device: device(4),
                permissions: Permissions {
                    control: false,
                    unattended: false,
                },
            },
            Request::PairingsRename {
                device: device(4),
                name: "desk phone".to_owned(),
            },
            Request::SessionsKill { id: SessionId(7) },
            Request::RelayAdd {
                relay: RelayAddress {
                    address: "relay.example:47990".to_owned(),
                    key: Some(device(5)),
                },
            },
            Request::RelayRemove {
                address: "relay.example:47990".to_owned(),
            },
            Request::Doctor,
        ];
        for request in requests {
            let bytes = postcard::to_stdvec(&request).expect("a request encodes");
            assert_eq!(
                postcard::from_bytes::<Request>(&bytes).expect("and decodes"),
                request
            );
        }
    }

    #[tokio::test]
    async fn a_message_split_across_reads_is_read_whole() {
        let (mine, theirs) = UnixStream::pair().expect("a socket pair");
        let mut wire = Wire::new(mine);
        let response = Response::Pairings(vec![pairing(1), pairing(2), pairing(3)]);

        let mut framed = vec![0u8; HEADER];
        postcard::to_io(&response, &mut framed).expect("a response encodes");
        let len = u32::try_from(framed.len() - HEADER).expect("a short message");
        framed[..HEADER].copy_from_slice(&len.to_be_bytes());

        // Two bytes at a time, which splits the length prefix as well as the
        // body. A reader that framed over the stream rather than over a buffer
        // would lose its place at the first of these.
        let writer = tokio::spawn(async move {
            let mut theirs = theirs;
            for chunk in framed.chunks(2) {
                theirs.write_all(chunk).await.expect("the pair takes bytes");
                tokio::task::yield_now().await;
            }
            theirs
        });
        assert_eq!(
            wire.recv::<Response>().await.expect("one message"),
            response
        );
        drop(writer.await.expect("the writer finishes"));
    }

    #[tokio::test]
    async fn two_messages_that_arrive_in_one_read_are_two_messages() {
        let (mine, theirs) = UnixStream::pair().expect("a socket pair");
        let mut wire = Wire::new(mine);
        let mut sender = Wire::new(theirs);
        sender.send(&Response::Done).await.expect("the first goes");
        sender
            .send(&Response::Failed {
                reason: "no".to_owned(),
            })
            .await
            .expect("and the second");
        assert_eq!(wire.recv::<Response>().await.unwrap(), Response::Done);
        assert_eq!(
            wire.recv::<Response>().await.unwrap(),
            Response::Failed {
                reason: "no".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_length_nobody_could_mean_is_refused_before_it_is_allocated_for() {
        let (mine, mut theirs) = UnixStream::pair().expect("a socket pair");
        let mut wire = Wire::new(mine);
        theirs
            .write_all(&u32::MAX.to_be_bytes())
            .await
            .expect("the pair takes bytes");
        assert!(matches!(
            wire.recv::<Response>().await,
            Err(ControlError::TooLong { .. })
        ));
    }

    #[tokio::test]
    async fn a_socket_nobody_is_listening_on_says_the_daemon_is_not_running() {
        let path = std::env::temp_dir().join(format!("tachyx-absent-{}.sock", std::process::id()));
        assert!(matches!(
            connect(&path).await,
            Err(ControlError::NotRunning)
        ));
    }

    #[tokio::test]
    async fn a_socket_left_behind_by_a_daemon_that_died_is_taken_over() {
        let directory = std::env::temp_dir().join(format!("tachyx-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("hostd.sock");
        drop(listen(&path).await.expect("the first bind"));
        // The file is still there: dropping a listener does not unlink it.
        assert!(path.exists());
        let second = listen(&path).await.expect("the stale socket is taken over");
        assert!(
            matches!(listen(&path).await, Err(ControlError::Taken(_))),
            "a socket somebody is listening on is a second daemon"
        );
        drop(second);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn the_socket_and_its_directory_are_reachable_by_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = std::env::temp_dir().join(format!("tachyx-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("hostd.sock");
        let listener = listen(&path).await.expect("a socket");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "a directory at 0600 could not be entered at all, so this is 0700"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn a_control_directory_anybody_can_enter_is_narrowed_before_anything_is_bound_in_it() {
        use std::os::unix::fs::PermissionsExt as _;

        // What a build that made the directory and then chmodded it left behind
        // under a permissive umask, and what `connect(2)` needs to reach a
        // socket inside: nothing more than the right to enter.
        let directory = std::env::temp_dir().join(format!("tachyx-wide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("a directory");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777))
            .expect("a wide directory");

        let listener = listen(&directory.join("hostd.sock"))
            .await
            .expect("a socket");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "the socket's own mode is not the boundary: the directory is"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn a_symlink_where_the_control_directory_goes_is_refused_rather_than_followed() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = std::env::temp_dir().join(format!("tachyx-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let target = scratch.join("somebody-elses");
        std::fs::create_dir_all(&target).expect("a directory");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("0755");
        let planted = scratch.join("tachyx");
        std::os::unix::fs::symlink(&target, &planted).expect("a planted symlink");

        assert!(
            matches!(
                listen(&planted.join("hostd.sock")).await,
                Err(ControlError::Symlinked(_))
            ),
            "a symlink is followed by set_permissions, so what it points at would be chmodded"
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755,
            "and this is the mode that would have been changed"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn a_connection_from_this_same_user_is_one_the_daemon_answers() {
        let (mine, theirs) = UnixStream::pair().expect("a socket pair");
        assert!(is_own_user(&mine), "both ends of this pair are this process");
        drop(theirs);
    }
}
