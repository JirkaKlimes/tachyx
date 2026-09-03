//! The daemon: what `tachyx-hostd` does when nobody names a subcommand.
//!
//! It owns the device key, the list of who may reach this host, the sessions
//! running now, and the control socket every other subcommand talks over.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tachyx_crypto::{CryptoError, DeviceKey, PairedClients};
use tachyx_proto::control::{Codec, Codecs};
use tachyx_relay::{Arrivals, HostRendezvous, RelayClient, Role, Routes};
use tachyx_transport::endpoint::{Host, Tuning};
use tachyx_transport::TransportError;
use tokio::net::UnixListener;

use crate::config::{Config, ConfigError, PROVISIONAL_RELAY};
use crate::control::{self, ControlError, Pairing, PairingEvent, Request, Response, Status, Wire};
use crate::relay::{Home, Presence, MAILBOX_LIFETIME};
use crate::sessions::{Peer, Sessions};
use crate::{doctor, pairing, session};

/// The file the paired clients are kept in, inside the state directory.
const CLIENTS_FILE: &str = "clients.toml";

/// Where the paired clients are kept.
#[must_use]
pub fn clients_path(state: &Path) -> PathBuf {
    state.join(CLIENTS_FILE)
}

/// What a probe found, named the way the wire names a codec.
#[must_use]
pub fn codec_names(found: Codecs) -> Vec<String> {
    Codec::PREFERENCE
        .into_iter()
        .filter(|codec| found.contains(*codec))
        .map(|codec| codec.to_string())
        .collect()
}

/// Everything one daemon holds.
pub struct Daemon {
    /// Behind a lock because `relay add` edits the file and the daemon then
    /// reads it again, while sessions are being served from it.
    config: Mutex<Config>,
    /// Where the configuration is, because the CLI edits it in place.
    pub config_path: PathBuf,
    /// Where the key and the paired list are.
    pub state: PathBuf,
    /// This host's own key. Its public half is this host's name.
    ///
    /// Shared because the TLS configuration, the relay client and the
    /// rendezvous all sign with it, and one key read once is one key.
    pub key: Arc<DeviceKey>,
    /// Who may reach this host, and what each may do.
    ///
    /// Shared because the client certificate verifier reads it **inside** the
    /// handshake, so revoking a device takes effect on that device's next
    /// connection rather than on the next restart.
    pub clients: Arc<PairedClients>,
    /// The one endpoint and the one relay connection, once the session side has
    /// built them. Pairing shares them rather than opening a second socket.
    home: std::sync::OnceLock<Arc<Home>>,
    /// What the encoders offered when they were probed at startup.
    pub encoders: Codecs,
    /// Whether this host had sound to stream when it was probed at startup.
    ///
    /// It is what every hello announces, so it is fixed for the daemon's life:
    /// a client is told once, and a host that changed its mind halfway through
    /// a session would be a control that stops working.
    pub audio: bool,
    /// The sessions running now.
    pub sessions: Arc<Sessions>,
    /// What is known about the configured relays.
    pub presence: Presence,
    /// When this daemon started.
    started: Instant,
    /// Held for as long as a code is on offer, so that there is one code at a
    /// time. Two codes on one screen is two mailboxes and one confused person.
    pairing: tokio::sync::Mutex<()>,
}

/// Written by hand, and it names the key rather than printing it.
///
/// [`DeviceKey`] derives its own `Debug`, which prints the PKCS#8 the private
/// key is in. Nothing in this daemon may put that in a log.
impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("device", &self.key.id())
            .field("state", &self.state)
            .field("sessions", &self.sessions.count())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Reads the configuration, the key and the paired list.
    ///
    /// `encoders` and `audio` are what the caller probed, rather than probes of
    /// its own: they want a GStreamer registry and a sound server, and this
    /// wants to be constructible without either.
    ///
    /// # Errors
    /// If the configuration is not readable, or the key or the paired list
    /// cannot be opened.
    pub fn open(
        config_path: PathBuf,
        state: PathBuf,
        encoders: Codecs,
        audio: bool,
    ) -> Result<Arc<Self>, DaemonError> {
        let config = Config::load(&config_path)?;
        std::fs::create_dir_all(&state)?;
        let key = Arc::new(DeviceKey::load_or_create(&state)?);
        let clients = Arc::new(PairedClients::open(clients_path(&state))?);
        let presence = Presence::of(&config.relays);
        Ok(Arc::new(Self {
            config: Mutex::new(config),
            config_path,
            state,
            key,
            clients,
            home: std::sync::OnceLock::new(),
            encoders,
            audio,
            sessions: Arc::new(Sessions::new()),
            presence,
            started: Instant::now(),
            pairing: tokio::sync::Mutex::new(()),
        }))
    }

    /// Records the endpoint and relay connection the session side built, so
    /// that a pairing runs over the same socket.
    ///
    /// Set once, by the session task, before it accepts anything.
    pub fn settle(&self, home: Arc<Home>) {
        let _ = self.home.set(home);
    }

    /// The endpoint and relay connection, once the session side is up.
    ///
    /// `None` before then, which is a pairing asked for in the moment between
    /// the control socket opening and the endpoint binding.
    #[must_use]
    pub fn home(&self) -> Option<&Arc<Home>> {
        self.home.get()
    }

    /// The configuration as it stands.
    #[must_use]
    pub fn config(&self) -> Config {
        guard(&self.config).clone()
    }

    /// What `status` reports.
    #[must_use]
    pub fn status(&self) -> Status {
        let config = self.config();
        Status {
            device: self.key.id(),
            name: config.host_name(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime: self.started.elapsed().as_secs(),
            listen: config.listen.to_string(),
            encoders: codec_names(self.encoders),
            paired: self.clients.list().len(),
            relays: self.presence.snapshot(),
            sessions: self.sessions.list(),
        }
    }

    /// Edits the configuration file and reads it back, with nothing else
    /// touching it in between.
    ///
    /// `edit` is a read-modify-write of a file and `answer` runs one task a
    /// connection, so two `relay add`s that overlap would otherwise both write
    /// what they each read and one of the two would be lost. The lock this
    /// daemon already holds the configuration behind is the one that has to
    /// cover the whole of it: the file, the reload, and the list `status` reads
    /// are one thing.
    ///
    /// # Errors
    /// Whatever `edit` refuses, or a file that will not read back.
    fn edit_config<T>(
        &self,
        edit: impl FnOnce(&Path) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        let mut held = guard(&self.config);
        let outcome = edit(&self.config_path)?;
        let fresh = Config::load(&self.config_path)?;
        self.presence.reconfigure(&fresh.relays);
        *held = fresh;
        Ok(outcome)
    }
}

/// Runs the daemon until it is stopped or the control socket is lost.
///
/// # Errors
/// If there is no encoder, the compositor will not take injection, the state
/// directory is unreachable, or another daemon already holds the socket.
pub fn run() -> Result<(), DaemonError> {
    // Probed here rather than when a client asks. The set comes from the
    // GStreamer registry, so a daemon started outside the environment that
    // holds the plugins finds nothing and can encode nothing, and every client
    // that connects is then refused with "this host encodes nothing the client
    // can decode": a sentence about the client, for a fault in the host, an
    // hour of debugging away from the machine that has it.
    let encoders = tachyx_media::encoders()?;
    if encoders.best().is_none() {
        return Err(DaemonError::NoEncoder);
    }
    // Sound is optional and the picture is not, so this is a probe rather than
    // a requirement. It is asked once, here, because the answer travels in
    // every hello: a client draws its volume control from it, and a control
    // that does nothing is worse than no control.
    let audio = match crate::audio::probe() {
        Ok(sink) => {
            tracing::info!(monitor = sink.monitor(), "this host can stream its own sound");
            true
        }
        Err(error) => {
            tracing::info!(%error, "this host streams no sound");
            false
        }
    };
    let daemon = Daemon::open(
        Config::default_path()?,
        Config::state_directory()?,
        encoders,
        audio,
    )?;
    let config = daemon.config();
    tracing::info!(
        encoders = codec_names(encoders).join(", "),
        device = %daemon.key.id(),
        "tachyx-hostd starting"
    );
    announce(&daemon, &config);

    // Injection owns a Wayland connection and a thread of its own, so that a
    // stall in it holds up neither capture nor the session it serves. Started
    // once and shared by every session: reaching the compositor costs several
    // round trips, and the keyboard modifier state it holds is the state of a
    // real keyboard, which does not belong to any one client.
    let injection = tachyx_input::Injection::spawn(config.output.as_deref().unwrap_or("eDP-1"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(daemon, injection))
}

/// Says out loud what a person would otherwise find out from a session that
/// does not happen.
fn announce(daemon: &Daemon, config: &Config) {
    if config.relays.is_empty() {
        tracing::warn!(
            provisional = PROVISIONAL_RELAY,
            "no relay is configured: this host can be reached only from this network and can \
             pair with nobody. `tachyx-hostd relay add <address:port#KEY>` is how one gets set"
        );
    }
    if daemon.clients.is_empty() {
        tracing::warn!("no device is paired: this host accepts nobody. run `tachyx-hostd pair`");
    }
}

/// Serves the control socket and the sessions, until either ends.
async fn serve(daemon: Arc<Daemon>, injection: tachyx_input::Injection) -> Result<(), DaemonError> {
    let path = control::socket_path();
    let listener = control::listen(&path).await?;
    tracing::info!(socket = %path.display(), "control socket open");
    tokio::try_join!(
        answer(&daemon, listener),
        serve_sessions(&daemon, injection)
    )?;
    Ok(())
}

/// The most control connections at once.
///
/// Each is a task and a buffer that a message may grow to [`control::
/// MAX_MESSAGE`] bytes, and nothing else on this socket is slow. A person runs
/// one command at a time and a script runs a few; this is the number at which
/// something is wrong rather than busy.
const CONNECTIONS: usize = 64;

/// How long a connection has to finish the request it opened with.
///
/// A caller that announces a length and then says nothing holds a reader and
/// its buffer for as long as it is left there. The request is already written
/// by the time the CLI connects, so this is generous by three orders of
/// magnitude. Only the request is bounded: `pair` is answered over minutes by
/// design.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// Answers everything the CLI asks, one task per connection.
///
/// Public because a test serves the same socket without a daemon around it,
/// which is how every subcommand is exercised end to end.
///
/// # Errors
/// Nothing here ends the loop, so this returns only if the loop is changed to.
///
/// # Panics
/// If the semaphore that bounds the connections is closed. It is this
/// function's own and nothing closes it.
pub async fn answer(daemon: &Arc<Daemon>, listener: UnixListener) -> Result<(), DaemonError> {
    let room = Arc::new(tokio::sync::Semaphore::new(CONNECTIONS));
    loop {
        // Taken before the connection is accepted and held for as long as it
        // lives, so that a caller which opens thousands and then waits is
        // thousands of connections in a backlog rather than thousands of
        // buffers in this process.
        let permit = Arc::clone(&room)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        // A connection that fails to accept is that caller's problem. Only
        // something that ends the loop would leave the daemon unmanageable, and
        // nothing here does.
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "a control connection could not be accepted");
                continue;
            }
        };
        if !control::is_own_user(&stream) {
            tracing::warn!("a control connection from another user was refused");
            continue;
        }
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move {
            let _held = permit;
            if let Err(error) = handle(&daemon, &mut Wire::new(stream)).await {
                tracing::debug!(%error, "a control connection ended");
            }
        });
    }
}

/// Reads one request and answers it.
async fn handle(daemon: &Arc<Daemon>, wire: &mut Wire) -> Result<(), ControlError> {
    let request: Request = tokio::time::timeout(REQUEST_DEADLINE, wire.recv())
        .await
        .map_err(|_| ControlError::TimedOut)??;
    match request {
        Request::Id => wire.send(&Response::Id(daemon.key.id())).await,
        Request::Status => wire.send(&Response::Status(daemon.status())).await,
        Request::Pair { permissions, ttl } => {
            let Ok(_held) = daemon.pairing.try_lock() else {
                return wire
                    .send(&Response::Pairing(PairingEvent::Failed {
                        reason: "a pairing code is already on offer: use it, or wait for it to \
                                 expire"
                            .to_owned(),
                    }))
                    .await;
            };
            // Bounded here as well as in the CLI, because a relay closes a
            // mailbox after five minutes and a longer offer would be a promise
            // this host cannot keep.
            let ttl = Duration::from_secs(u64::from(ttl)).min(MAILBOX_LIFETIME);
            pairing::run(daemon, permissions, ttl, wire).await
        }
        Request::PairingsList => {
            let pairings = daemon.clients.list().iter().map(Pairing::of).collect();
            wire.send(&Response::Pairings(pairings)).await
        }
        Request::PairingsRevoke { device } => {
            reply(
                wire,
                daemon.clients.remove(device),
                "no such device is paired",
            )
            .await
        }
        Request::PairingsSet {
            device,
            permissions,
        } => {
            reply(
                wire,
                daemon.clients.set_permissions(device, permissions),
                "no such device is paired",
            )
            .await
        }
        Request::PairingsRename { device, name } => {
            reply(
                wire,
                daemon.clients.rename(device, pairing::label(&name)),
                "no such device is paired",
            )
            .await
        }
        Request::SessionsKill { id } => {
            let response = if daemon.sessions.kill(id) {
                Response::Done
            } else {
                Response::Failed {
                    reason: format!("no session {id} is running"),
                }
            };
            wire.send(&response).await
        }
        Request::RelayList => wire.send(&Response::Relays(daemon.config().relays)).await,
        Request::RelayAdd { relay } => {
            let response = match daemon.edit_config(|path| Config::add_relay(path, &relay)) {
                Ok(()) => Response::Done,
                Err(error) => Response::Failed {
                    reason: error.to_string(),
                },
            };
            wire.send(&response).await
        }
        Request::RelayRemove { address } => {
            let changed = daemon.edit_config(|path| Config::remove_relay(path, &address));
            reply(wire, changed, "no relay at that address is configured").await
        }
        Request::Doctor => {
            // Off the runtime's workers: it asks the compositor what it offers,
            // which is a blocking round trip.
            let held = Arc::clone(daemon);
            let report = tokio::task::spawn_blocking(move || doctor::of_daemon(&held))
                .await
                .map_err(|_| ControlError::Unexpected)?;
            wire.send(&Response::Doctor(report)).await
        }
    }
}

/// Reports a change that either happened, did not, or could not.
async fn reply<E: std::fmt::Display>(
    wire: &mut Wire,
    change: Result<bool, E>,
    missing: &str,
) -> Result<(), ControlError> {
    let response = match change {
        Ok(true) => Response::Done,
        Ok(false) => Response::Failed {
            reason: missing.to_owned(),
        },
        Err(error) => Response::Failed {
            reason: error.to_string(),
        },
    };
    wire.send(&response).await
}

/// Serves the sessions themselves.
///
/// One socket carries everything: the endpoint sessions arrive on, the
/// connection to the home relay, and every punch. `spec/relay.md` is the reason,
/// and it is not a tidiness argument: the address a relay observes is the
/// address a peer will be punched at, so a second socket would be observed at an
/// address nothing can reach.
async fn serve_sessions(
    daemon: &Arc<Daemon>,
    injection: tachyx_input::Injection,
) -> Result<(), DaemonError> {
    let config = daemon.config();
    let routes = Arc::new(Routes::new());
    let server = tachyx_transport::server_config(
        &daemon.key,
        Arc::clone(&daemon.clients),
        tuning(),
    )?;
    let (endpoint, socket) = tachyx_relay::endpoint(config.listen, Some(server), &routes)?;

    // The first configured relay, and only the first. A host that registered
    // with several would be reachable through all of them, and a client would
    // then have to be told which, so the home relay is one by construction.
    let relay = config.relays.first().map(|address| {
        (
            address.clone(),
            Arc::new(RelayClient::open(
                address.clone(),
                Role::Host,
                Arc::clone(&daemon.key),
                endpoint.clone(),
                Arc::clone(&socket),
            )),
        )
    });
    let home = Arc::new(Home::new(
        endpoint.clone(),
        socket,
        Arc::clone(&routes),
        Arc::new(Arrivals::new()),
        relay,
    ));
    daemon.settle(Arc::clone(&home));

    tracing::info!(
        address = %endpoint.local_addr()?,
        device = %daemon.key.id(),
        "tachyx-hostd listening"
    );
    if daemon.clients.is_empty() {
        tracing::warn!(
            "no device is paired, so this host accepts nobody. run `tachyx-hostd pair`"
        );
    }

    if let Some(client) = home.relay() {
        spawn_presence(daemon, &home, Arc::clone(client));
    } else {
        tracing::warn!(
            "no relay is configured, so this host can be reached only from this network \
             and can pair with nobody. run `tachyx-hostd relay add`"
        );
    }

    let host = Host::on_endpoint(endpoint, tuning()).with_audio(daemon.audio);
    loop {
        // A connection that fails to establish is that client's problem, not the
        // daemon's. Only losing the socket itself ends the loop.
        let session = match host.accept().await {
            Ok(session) => session,
            Err(TransportError::Closed) => return Err(DaemonError::Closed),
            Err(error) => {
                tracing::warn!(%error, "a client failed to connect");
                continue;
            }
        };
        // The punch is told at once. A throwaway attempt that is not stopped
        // runs its whole budget and keeps sending Initials at a peer that has
        // already arrived.
        home.arrivals().note(session.peer);
        let peer = Peer {
            device: session.peer,
            name: daemon
                .clients
                .get(session.peer)
                .map_or_else(|| session.peer.to_string(), |paired| paired.name),
            // A peer the routing table has a virtual address for arrived
            // through a relay. One it does not came straight off the wire.
            relayed: routes.route(session.connection.remote_address()).is_some(),
        };
        tracing::info!(device = %peer.device, name = %peer.name, relayed = peer.relayed, "client connected");
        match session::serve(
            &daemon.config(),
            session,
            &injection,
            &daemon.sessions,
            peer.clone(),
            daemon.audio,
        )
        .await
        {
            Ok(()) => tracing::info!(device = %peer.device, "client disconnected"),
            Err(error) => tracing::warn!(device = %peer.device, %error, "session ended"),
        }
    }
}

/// Keeps the home relay connection, and answers what arrives on it.
///
/// Two jobs on one task because both read the same connection: presence is what
/// makes this host findable, and the rendezvous is what answers a client that
/// found it. An offer that is not answered is a client that cannot connect.
fn spawn_presence(daemon: &Arc<Daemon>, home: &Arc<Home>, client: Arc<RelayClient>) {
    let daemon = Arc::clone(daemon);
    let home = Arc::clone(home);
    tokio::spawn(async move {
        let address = home.address().cloned();
        let rendezvous = HostRendezvous::new(
            Arc::clone(&daemon.key),
            Arc::clone(&daemon.clients),
            Arc::clone(&client),
            home.endpoint().clone(),
            Arc::clone(home.arrivals()),
            tuning(),
        );
        loop {
            match client.wait_observed(REACH).await {
                Ok(observed) => {
                    tracing::info!(%observed, "the relay sees this host at this address");
                    if let Some(address) = &address {
                        daemon.presence.observed(
                            address,
                            true,
                            client
                                .round_trip()
                                .map(|trip| u32::try_from(trip.as_micros()).unwrap_or(u32::MAX)),
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot reach the home relay");
                    if let Some(address) = &address {
                        daemon.presence.observed(address, false, None);
                    }
                }
            }
            // Runs until the relay connection is gone for good. The client
            // reconnects underneath, so returning here means it gave up.
            let error = rendezvous.serve(&home.endpoints(client.observed())).await;
            tracing::warn!(%error, "the home relay connection ended");
            if let Some(address) = &address {
                daemon.presence.observed(address, false, None);
            }
            tokio::time::sleep(RETRY).await;
        }
    });
}

/// How long the relay is given to report this host's address before the wait is
/// reported as a relay that cannot be reached.
const REACH: std::time::Duration = std::time::Duration::from_secs(5);

/// How long before the home relay is tried again after it gave up.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// The transport settings every session on this host runs with.
///
/// The buffer is sized for 1080p and not for the largest picture imaginable. The
/// send gate's bound **is** this buffer, so a generous one is not left unused:
/// it is filled, and standing queue is latency. At 3840x2160 it came to 809 kB,
/// two thirds of a second of video waiting to be sent, which the congestion
/// controller reads as bandwidth.
///
/// The path is treated as one that may be relayed, which lowers the initial
/// maximum transmission unit. A session that turns out to be direct discovers
/// the larger figure; a session that is relayed and started at the local network
/// figure would have every packet black-holed until discovery noticed.
fn tuning() -> Tuning {
    Tuning::for_stream(
        tachyx_proto::control::CodedSize::new(1920, 1088).expect("1920x1088 is codeable"),
    )
    .relayed()
}

/// The configuration, whether or not a thread died holding it.
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why the daemon stopped, or would not start.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The configuration could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The key or the paired list could not be opened.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// The control socket could not be opened.
    #[error(transparent)]
    Control(#[from] ControlError),
    /// The GStreamer registry could not be read.
    #[error(transparent)]
    Media(#[from] tachyx_media::Error),
    /// The compositor will not take injected input.
    #[error(transparent)]
    Input(#[from] tachyx_input::Error),
    /// The state directory, or the runtime, could not be made.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The endpoint sessions arrive on could not be built.
    #[error(transparent)]
    Transport(#[from] tachyx_transport::TransportError),
    /// The relay client or the routing socket refused.
    #[error(transparent)]
    Relay(#[from] tachyx_relay::RelayError),
    /// The endpoint was closed, which is the only thing that ends the session
    /// loop.
    #[error("the endpoint sessions arrive on was closed")]
    Closed,
    /// There is nothing to encode with, so no session could ever carry a
    /// picture.
    #[error(
        "no video encoder: the gstreamer registry offers none of vah264enc, vah264lpenc, \
         vaav1enc or vaav1lpenc. the plugins are missing from this environment (run inside \
         `nix develop`), or this machine has no va-api"
    )]
    NoEncoder,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tachyx-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn a_daemon_that_has_never_run_here_names_itself_and_has_nobody_paired() {
        let state = scratch("open");
        let daemon = Daemon::open(state.join("hostd.toml"), state.clone(), Codecs::NONE, false)
            .expect("a daemon opens on an empty directory");
        let status = daemon.status();
        assert_eq!(status.device, daemon.key.id());
        assert_eq!(status.paired, 0);
        assert!(status.sessions.is_empty());
        assert_eq!(
            status.relays.len(),
            1,
            "a fresh host starts with the provisional relay, or it can pair with nobody"
        );
        assert!(!status.relays[0].connected, "nothing has reached it yet");
        assert!(status.encoders.is_empty(), "nothing was probed");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_is_announced_and_never_sent_does_not_hold_the_reader_for_ever() {
        use tokio::io::AsyncWriteExt as _;

        let state = scratch("deadline");
        let daemon =
            Daemon::open(state.join("hostd.toml"), state.clone(), Codecs::NONE, false).expect("a daemon");
        let (mine, mut theirs) = tokio::net::UnixStream::pair().expect("a socket pair");
        let served = tokio::spawn(async move { handle(&daemon, &mut Wire::new(mine)).await });

        // The largest message the framing takes, and eight bytes of it. The
        // reader waits for the rest with a buffer it has already been told the
        // size of.
        let announced = u32::try_from(control::MAX_MESSAGE).expect("a length");
        theirs.write_all(&announced.to_be_bytes()).await.unwrap();
        theirs.write_all(&[0u8; 8]).await.unwrap();

        assert!(
            matches!(
                served.await.expect("the task finishes"),
                Err(ControlError::TimedOut)
            ),
            "a connection that says nothing more holds a task and its buffer until it is closed"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_daemon_never_prints_its_own_key() {
        let state = scratch("debug");
        let daemon =
            Daemon::open(state.join("hostd.toml"), state.clone(), Codecs::NONE, false).expect("a daemon");
        let printed = format!("{daemon:?}");
        assert!(printed.contains(&daemon.key.id().to_string()));
        assert!(
            !printed.contains("pkcs8"),
            "the derived Debug of a DeviceKey prints the private key: {printed}"
        );
        let _ = std::fs::remove_dir_all(&state);
    }
}
