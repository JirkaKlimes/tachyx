//! The client's I/O driver.
//!
//! `tachyx-client` decides what a session does and touches nothing;
//! `tachyx-transport` moves bytes and decides nothing. This is the loop between
//! them: it reaches a host, hands the session the messages that arrive, writes
//! the messages the session produces, and gives complete pictures to a decoder.
//!
//! It is the only place in the client that owns a clock, a socket and a
//! runtime, which is what keeps the two crates under it testable without any of
//! the three.
//!
//! **A host is reached by device, not by address.** Both ends check a key, so an
//! address alone opens nothing: the host must hold this client's device in its
//! paired list and this client must hold the host's. [`pairing`] is how a device
//! gets into those lists, and [`Link::connect`] is how one is reached afterwards,
//! over an address the host gave at pairing or through a relay when it has
//! moved.
//!
//! The decoder is reached through [`FrameSink`] rather than named directly, and
//! the speakers through [`AudioSink`]. Linux decodes through GStreamer into a
//! Flutter texture and Android through the platform decoder into a surface
//! producer; neither belongs in a loop whose whole job is protocol.

#![forbid(unsafe_code)]

pub mod pairing;
mod run;

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use quinn::{Connection, Endpoint, VarInt};
use tachyx_client::{AudioEvent, CursorView, Liveness, Session};
use tachyx_crypto::store::KnownHost;
use tachyx_crypto::{CryptoError, DeviceId, DeviceKey, KnownHosts, RelayAddress};
use tachyx_proto::control::{
    AudioCommand, AudioMode, AudioStatus, Capabilities, ClientHello, CodedSize, Heartbeat, Size,
    StreamConfig,
};
use tachyx_proto::input::InputEvent;
use tachyx_proto::media::Timestamp;
use tachyx_proto::packet::ReassemblyStats;
use tachyx_relay::{ClientRendezvous, RelayClient, RelayError, Role, Routes, RoutingSocket};
use tachyx_transport::{Client, ClientSession, TransportError, Tuning};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

pub use pairing::{pair, PairError, PairFault, Paired};

/// The file the client keeps the hosts it knows in, inside its state directory.
///
/// Named here and nowhere else, so that pairing writes the list connecting
/// reads.
const HOSTS_FILE: &str = "hosts.toml";

/// The hosts this client is paired with, out of `state`.
///
/// # Errors
/// If the file exists and is not a list this build reads. A file somebody
/// hand-edited into nonsense refuses rather than reading as nobody paired.
pub fn known_hosts(state: &Path) -> Result<KnownHosts, CryptoError> {
    KnownHosts::open(state.join(HOSTS_FILE))
}

/// How long one of the host's own addresses is given before a relay is asked.
///
/// A handshake on a local network completes in a few milliseconds. This is long
/// enough for a slow access point and short enough that a host which has moved
/// costs a fraction of a second before the relay is tried.
const NEARBY: Duration = Duration::from_millis(600);

/// How long a relay is given to answer before the next one is tried.
///
/// The relay reports the address it sees as soon as the connection is up, so
/// this is how long the whole connection has to happen in.
const REACH: Duration = Duration::from_secs(5);

/// The close code a client that is leaving of its own accord uses.
const GOODBYE: VarInt = VarInt::from_u32(0);

/// How long the runtime is given to put the closing frame on the wire.
///
/// Without it the host learns of the departure from its idle timeout five
/// seconds later, and reports a lost client where there was a polite one.
const GOODBYE_FLUSH: Duration = Duration::from_millis(100);

/// Where complete pictures go.
///
/// One method, because a decoder is all this needs to know about: the geometry
/// travels in the bitstream and the aspect ratio is the application's business,
/// not the decoder's.
pub trait FrameSink: fmt::Debug + Send + Sync {
    /// Takes one complete access unit, in decode order.
    ///
    /// Called from the receive loop, so it must not block for long: everything
    /// behind it, input included, waits.
    fn push(&self, access_unit: Vec<u8>);
}

/// Where sound goes.
///
/// Named rather than reached directly for the same reason [`FrameSink`] is: a
/// loop whose whole job is protocol has no business knowing which sound server
/// this machine runs. What is behind it is `tachyx-audio`.
pub trait AudioSink: fmt::Debug + Send + Sync {
    /// Takes one thing to play, in the order it was captured.
    ///
    /// A hole in the sound arrives as [`AudioEvent::Lost`] ahead of the packet
    /// behind it, so what receives this is told how much sound to invent rather
    /// than left to work it out from a gap in the timing.
    ///
    /// Called from the receive loop, so it must not block for long: everything
    /// behind it, the picture included, waits.
    fn push(&self, event: AudioEvent);
}

/// What one session has done, as the client itself measured it.
///
/// Nothing here is invented and nothing is second-hand except [`Stats::host`],
/// which is what the host says about itself and is labelled as such.
#[derive(Debug, Clone)]
pub struct Stats {
    /// Whether pictures are arriving, the screen is merely still, or the host
    /// has stopped answering.
    pub liveness: Liveness,
    /// The transport's own round trip estimate.
    ///
    /// quinn measures this from its acknowledgements, so it is the real path and
    /// costs no message of our own. It is a round trip: no part of it can be
    /// attributed to one direction without the two clocks being related, which
    /// this milestone does not do.
    pub rtt: Duration,
    /// What reassembly saw: packets, frames, and everything lost between them.
    pub video: ReassemblyStats,
    /// Whether the host said it can stream its own sound. It decides whether the
    /// application offers a control at all, and nothing else answers it.
    pub host_audio: bool,
    /// What the host last said it is doing about sound.
    pub audio: Option<AudioStatus>,
    /// Opus packets handed to the sink.
    pub audio_packets: u64,
    /// Frames of sound that never arrived, which whatever plays had to invent.
    pub audio_lost: u64,
    /// Input batches put on the wire.
    pub batches: u64,
    /// How the host said it is encoding, once it has said.
    pub stream: Option<StreamConfig>,
    /// The last thing the host said about itself.
    pub host: Option<Heartbeat>,
    /// Why the session ended, if it has.
    pub ended: Option<String>,
}

/// What the loop publishes and the handle reads.
#[derive(Debug)]
struct State {
    video: ReassemblyStats,
    host_audio: bool,
    audio: Option<AudioStatus>,
    audio_packets: u64,
    audio_lost: u64,
    batches: u64,
    stream: Option<StreamConfig>,
    beat: Option<Heartbeat>,
    heard: Instant,
    saw: Option<Instant>,
    ended: Option<String>,
    sink: Option<Arc<dyn FrameSink>>,
    speaker: Option<Arc<dyn AudioSink>>,
    cursor: Option<CursorView>,
}

/// The one thing the loop and the handle share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(State {
                video: ReassemblyStats::default(),
                host_audio: false,
                audio: None,
                audio_packets: 0,
                audio_lost: 0,
                batches: 0,
                stream: None,
                beat: None,
                heard: now,
                saw: None,
                ended: None,
                sink: None,
                speaker: None,
                cursor: None,
            }),
        }
    }

    /// The state, whether or not a thread died holding it.
    ///
    /// A panicked lock here means the loop stopped mid-publish, which loses one
    /// update of a statistics panel. Refusing to show the panel afterwards would
    /// be the larger fault.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Something for the loop to do that came from outside it.
#[derive(Debug)]
enum Command {
    /// The user did this at this point on the client's session clock.
    Input(Timestamp, InputEvent),
    /// The client can take something different now.
    Viewport(Capabilities),
    /// Complete pictures go here from now on.
    Present(Arc<dyn FrameSink>),
    /// Sound goes here from now on.
    Listen(Arc<dyn AudioSink>),
    /// What to ask the host to do about its sound.
    Audio(AudioCommand),
}

/// A live session with a host.
///
/// Dropping it closes the connection, so the host sees a departure rather than
/// an idle timeout.
#[derive(Debug)]
pub struct Link {
    connection: Connection,
    commands: mpsc::UnboundedSender<Command>,
    shared: Arc<Shared>,
    /// Taken in [`Drop`], which is the only place it is `None`.
    runtime: Option<Runtime>,
}

impl Link {
    /// Reaches `device` and runs the session.
    ///
    /// The device is looked up in the hosts this client paired with, in `state`.
    /// What that record holds is where to look: the addresses the host gave at
    /// pairing are tried first, briefly, because on one network they answer and
    /// no relay is involved. Only when none of them answers is a relay asked to
    /// arrange a meeting, the host's own relay first and then `relays`.
    ///
    /// `state` is this client's own directory. Its device key lives there, and
    /// that key is what the host compares against its paired list.
    ///
    /// # Errors
    /// If this client has not paired with `device`, the runtime will not start,
    /// the device key cannot be read, nothing that knows where the host is can
    /// be reached, or the host refuses the session. A refusal carries the host's
    /// own words.
    pub fn connect(
        device: DeviceId,
        state: &Path,
        relays: &[RelayAddress],
        capabilities: Capabilities,
    ) -> Result<Self, Error> {
        let known = known_hosts(state)?
            .get(device)
            .ok_or(Error::Unknown { device })?;
        let runtime = runtime()?;
        let now = Instant::now();
        let session = Session::new(capabilities, now);
        // All of it inside the runtime: a quinn endpoint binds its socket to
        // whichever reactor is current, and outside one there is none to bind
        // to.
        let (held, opened) = runtime.block_on(reach(
            &known,
            state,
            relays,
            tuning(capabilities.max_decode),
            &session.hello(),
        ))?;
        Ok(Self::running(runtime, session, held, opened, now))
    }

    /// Dials one address, with no pairing record and no relay.
    ///
    /// This is the development path and it is what `TACHYX_HOST` drives. It
    /// exists because a host on the bench is reached before anybody has paired
    /// with it, and because a fault in the rendezvous should be provable against
    /// a path that has none of it. It is not how the application connects: an
    /// address is not an identity and a person has no way to learn one.
    ///
    /// `address` is `host:port`, a name or a literal. `host` says who must
    /// answer, and it is the only half that is checked.
    ///
    /// # Errors
    /// If the address names nothing, the runtime will not start, the device key
    /// cannot be read, the peer at that address is a different device, or the
    /// host refuses the session.
    pub fn dial(
        address: &str,
        host: DeviceId,
        state: &Path,
        capabilities: Capabilities,
    ) -> Result<Self, Error> {
        let runtime = runtime()?;
        let now = Instant::now();
        let session = Session::new(capabilities, now);
        let (client, opened) = runtime.block_on(async {
            let socket = resolve(address).await?;
            let key = Arc::new(DeviceKey::load_or_create(state).map_err(TransportError::from)?);
            let client = Client::bind(key, tuning(capabilities.max_decode))?;
            let opened = client
                .connect(socket, host, &session.hello())
                .await
                .map_err(|error| refusal(host, error))?;
            Ok::<_, Error>((client, opened))
        })?;
        Ok(Self::running(
            runtime,
            session,
            Held::Nearby(client),
            opened,
            now,
        ))
    }

    /// Starts the loop over a session that is already open.
    fn running(
        runtime: Runtime,
        session: Session,
        held: Held,
        opened: ClientSession,
        now: Instant,
    ) -> Self {
        let connection = opened.connection.clone();
        let shared = Arc::new(Shared::new(now));
        let (commands, from_handle) = mpsc::unbounded_channel();
        runtime.spawn(run::session(
            session,
            opened,
            held,
            Arc::clone(&shared),
            from_handle,
        ));
        Self {
            connection,
            commands,
            shared,
            runtime: Some(runtime),
        }
    }

    /// What this session has done. See [`Stats`].
    #[must_use]
    pub fn stats(&self) -> Stats {
        let state = self.shared.state();
        Stats {
            liveness: Liveness::at(Instant::now(), state.heard, state.saw),
            rtt: self.connection.rtt(),
            video: state.video,
            host_audio: state.host_audio,
            audio: state.audio,
            audio_packets: state.audio_packets,
            audio_lost: state.audio_lost,
            batches: state.batches,
            stream: state.stream,
            host: state.beat,
            ended: state.ended.clone(),
        }
    }

    /// Where to draw the host's pointer, or `None` when there is nothing to
    /// draw.
    ///
    /// Read once a frame by whatever draws it. The position is the client's
    /// own, moved by the user's own input and corrected by what the host
    /// reports, so it costs no round trip and it is never behind the finger.
    #[must_use]
    pub fn cursor(&self) -> Option<CursorView> {
        self.shared.state().cursor.clone()
    }

    /// Sends the pictures that arrive from now on to `sink`.
    ///
    /// Separate from [`Link::connect`] because a Flutter texture can only be
    /// registered from the platform thread, which is not the thread a connection
    /// can be opened on. That gap is why this goes through the loop rather than
    /// writing the sink here: the loop also has to tell the host that a decoder
    /// now exists which has decoded nothing.
    pub fn present_on(&self, sink: Arc<dyn FrameSink>) {
        let _ = self.commands.send(Command::Present(sink));
    }

    /// What the host last said it is doing about sound.
    ///
    /// Published separately from [`Link::stats`] for the reason
    /// [`Link::cursor`] is: this is read on every packet of sound by whatever
    /// applies the volume, a hundred times a second, and the whole of the
    /// statistics is a round trip estimate and a string clone more than that
    /// reader needs.
    #[must_use]
    pub fn audio_status(&self) -> Option<AudioStatus> {
        self.shared.state().audio
    }

    /// Sends the sound that arrives from now on to `sink`.
    ///
    /// Separate from [`Link::connect`] for the smaller of the two reasons
    /// [`Link::present_on`] is: opening a sound device takes as long as the
    /// machine's sound server takes to answer, and a session should be running
    /// before that is discovered. Sound that arrives before a sink does is
    /// dropped rather than queued: it is a moment of a live stream, and a moment
    /// played late is worse than a moment not played.
    pub fn listen_on(&self, sink: Arc<dyn AudioSink>) {
        let _ = self.commands.send(Command::Listen(sink));
    }

    /// Asks the host to start streaming its sound, stop, or change what it does
    /// with its own output meanwhile.
    ///
    /// Ignored by a host that said it has no audio. What it is really doing
    /// comes back in [`Stats::audio`], because a host may refuse.
    pub fn set_audio_mode(&self, mode: AudioMode) {
        let _ = self
            .commands
            .send(Command::Audio(AudioCommand::SetMode(mode)));
    }

    /// Asks the host to set its own sink to this volume, in permille of
    /// [`tachyx_proto::control::VOLUME_NORM`].
    ///
    /// There is one volume in the session and it is the host's. What the client
    /// hears follows from it, so this is the only volume control the application
    /// needs.
    pub fn set_host_volume(&self, permille: u16) {
        let _ = self
            .commands
            .send(Command::Audio(AudioCommand::SetVolume(permille)));
    }

    /// Asks the host to mute or unmute its own sink.
    ///
    /// The host's silence, not the client's: the capture tap sits ahead of the
    /// mute, so a muted host still streams everything it is playing.
    pub fn set_host_muted(&self, muted: bool) {
        let _ = self
            .commands
            .send(Command::Audio(AudioCommand::SetMuted(muted)));
    }

    /// Records what the user did at `at` on the client's session clock.
    ///
    /// Nothing leaves here: the events are merged and go out on the next tick.
    /// Ignored once the session has ended, which is what the statistics say and
    /// this does not repeat.
    pub fn input(&self, at: Timestamp, event: InputEvent) {
        let _ = self.commands.send(Command::Input(at, event));
    }

    /// Tells the host the client can take something different now, after a
    /// rotation, a resize, or a change of resolution mode.
    pub fn viewport_changed(&self, capabilities: Capabilities) {
        let _ = self.commands.send(Command::Viewport(capabilities));
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.connection.close(GOODBYE, b"the client is leaving");
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(GOODBYE_FLUSH);
        }
    }
}

/// The transport settings for a client that asked for pictures no larger than
/// `viewport`.
///
/// The buffer has to hold a whole key frame or every key frame is shredded on
/// arrival and no picture is ever shown. The client cannot know the coded size
/// before the host has chosen one, but it does know the size it asked for, and
/// the host cannot exceed it.
fn tuning(viewport: Size) -> Tuning {
    let coded = CodedSize::new(
        align_up(viewport.width.get()),
        align_up(viewport.height.get()),
    )
    // A coded size is a multiple of sixteen and both terms are non-zero, which
    // is what the rounding above produces.
    .unwrap_or_else(|| CodedSize::new(1920, 1088).expect("1920x1088 is codeable"));
    Tuning::for_stream(coded)
}

/// The next multiple of the coded alignment at or above `pixels`.
fn align_up(pixels: u16) -> u16 {
    let alignment = CodedSize::ALIGNMENT;
    pixels
        .checked_next_multiple_of(alignment)
        .unwrap_or(u16::MAX / alignment * alignment)
}

/// Turns what the user typed into one address to dial.
async fn resolve(address: &str) -> Result<SocketAddr, Error> {
    if let Ok(socket) = address.parse() {
        return Ok(socket);
    }
    tokio::net::lookup_host(address)
        .await
        .ok()
        .and_then(|mut found| found.next())
        .ok_or_else(|| Error::Address {
            address: address.to_owned(),
        })
}

/// The client's own runtime.
///
/// Two worker threads: one for quinn's endpoint driver and one for the session
/// loop. A client runs exactly two tasks and a phone has better uses for its
/// cores.
///
/// Both are raised, because both are on the picture's path: one takes the
/// datagram off the socket and the other reassembles it and hands it to the
/// decoder. Neither ever computes for long, which is what makes them safe to
/// raise; see [`tachyx_sched`].
fn runtime() -> Result<Runtime, Error> {
    Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(|| tachyx_sched::raise("client"))
        .enable_all()
        .build()
        .map_err(Error::Runtime)
}

/// What one session runs on, held for as long as the session lives.
///
/// The endpoint outlives the connection it dialled, and a relayed session needs
/// its relay connection as well: a lane dies with the control connection that
/// asked for it, so dropping the relay client would cut a spliced session in
/// half.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "held so that it is dropped with the session and not before"
)]
enum Held {
    /// Straight to an address the host gave at pairing.
    Nearby(Client),
    /// Through a relay, whether the punch worked or the traffic is spliced.
    Rendezvous(ClientRendezvous),
}

/// One UDP socket for everything: the relay connection, the punch and the
/// session.
///
/// That is what makes the address a relay observes the address a host will be
/// reached at. IPv6 first, then IPv4 for a machine without it, which is what
/// [`tachyx_transport::bind_any`] does for the plain path.
fn bind(routes: &Arc<Routes>) -> Result<(Endpoint, Arc<RoutingSocket>), RelayError> {
    tachyx_relay::endpoint((Ipv6Addr::UNSPECIFIED, 0).into(), None, routes)
        .or_else(|_| tachyx_relay::endpoint((Ipv4Addr::UNSPECIFIED, 0).into(), None, routes))
}

/// Opens a session with `known`, on the wire if it can and through a relay if
/// it must.
async fn reach(
    known: &KnownHost,
    state: &Path,
    relays: &[RelayAddress],
    tuning: Tuning,
    hello: &ClientHello,
) -> Result<(Held, ClientSession), Error> {
    let key = Arc::new(DeviceKey::load_or_create(state).map_err(TransportError::from)?);
    let routes = Arc::new(Routes::new());
    let (endpoint, socket) = bind(&routes)?;

    let direct = Client::on_endpoint(endpoint.clone(), Arc::clone(&key), tuning);
    if let Some(session) = nearby(&direct, &routes, known, hello).await? {
        return Ok((Held::Nearby(direct), session));
    }

    // Relayed from here on, so the packets start at what a tailnet or a relayed
    // hop carries. A packet of the local network's size would be black-holed
    // until discovery noticed, which is a stall at the moment a session opens.
    let tuning = tuning.relayed();
    for relay in relays_for(known, relays) {
        let client = Arc::new(RelayClient::open(
            relay.clone(),
            Role::Client,
            Arc::clone(&key),
            endpoint.clone(),
            Arc::clone(&socket),
        ));
        if let Err(error) = client.wait_observed(REACH).await {
            tracing::debug!(relay = %relay.address, %error, "that relay did not answer");
            continue;
        }
        let rendezvous = ClientRendezvous::new(
            Arc::clone(&key),
            client,
            Arc::clone(&routes),
            Client::on_endpoint(endpoint.clone(), Arc::clone(&key), tuning),
        );
        // No candidates of this client's own: the addresses a client offers are
        // what the host punches toward, and the one that works from outside is
        // the one the relay observed, which the rendezvous adds itself. A host
        // on the same network was already reached above.
        match rendezvous.open(known.device, &[], hello).await {
            Ok((session, path)) => {
                tracing::info!(device = %known.device, ?path, "a session opened through a relay");
                return Ok((Held::Rendezvous(rendezvous), session));
            }
            Err(RelayError::Transport(error)) if !unanswered(&error) => {
                return Err(refusal(known.device, error));
            }
            Err(error) => {
                tracing::debug!(relay = %relay.address, %error, "that relay could not reach the host");
            }
        }
    }
    Err(Error::Unreachable {
        device: known.device,
    })
}

/// Tries the addresses the host gave at pairing, briefly.
///
/// `Ok(None)` means nothing at any of them answered, which is what sends the
/// caller to a relay. An error is a host that did answer and said no: a relay
/// would reach the same host and be told the same thing, and would say it worse.
async fn nearby(
    client: &Client,
    routes: &Routes,
    known: &KnownHost,
    hello: &ClientHello,
) -> Result<Option<ClientSession>, Error> {
    for address in &known.endpoints {
        // A virtual address even for a direct path, so that the route under a
        // live session can be moved later without quinn seeing a migration.
        let known_as = routes.direct(*address);
        match tokio::time::timeout(NEARBY, client.connect(known_as, known.device, hello)).await {
            Ok(Ok(session)) => return Ok(Some(session)),
            Ok(Err(error)) if !unanswered(&error) => return Err(refusal(known.device, error)),
            Ok(Err(error)) => tracing::debug!(%address, %error, "that address did not answer"),
            Err(_) => tracing::debug!(%address, "that address did not answer in time"),
        }
        routes.forget(known_as);
    }
    Ok(None)
}

/// The relays to ask, the host's own first.
///
/// The host wrote its own relay into the pairing, so that is where it is
/// registered. The configured ones follow, for a host that has moved to one of
/// them since.
fn relays_for<'a>(
    known: &'a KnownHost,
    configured: &'a [RelayAddress],
) -> impl Iterator<Item = &'a RelayAddress> {
    known.relay.iter().chain(configured.iter().filter(|relay| {
        known
            .relay
            .as_ref()
            .is_none_or(|home| home.address != relay.address)
    }))
}

/// Whether the error says nothing was at that address, as against a host that
/// answered and said something.
///
/// The difference decides whether there is any point in trying somewhere else.
fn unanswered(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Socket(_)
            | TransportError::Connect(_)
            | TransportError::Connection(
                quinn::ConnectionError::TimedOut
                    | quinn::ConnectionError::LocallyClosed
                    | quinn::ConnectionError::Reset
            )
    )
}

/// What a host that answered and refused is, in words a person can act on.
///
/// The host checks its paired list **inside** the handshake, and in TLS 1.3 the
/// client's certificate travels in the client's own second flight. So quinn
/// reports the handshake complete to this end before the host has looked at it,
/// and the refusal arrives as the connection's close on the first read. That
/// close carries a TLS alert, and an alert is the only thing this configuration
/// can fail on: the certificates are self-signed and neither end reads a name or
/// a date. So an alert here means one thing, and it is worth saying rather than
/// reporting a read that failed.
fn refusal(device: DeviceId, error: TransportError) -> Error {
    /// The first QUIC transport error code that carries a TLS alert.
    const ALERTS: std::ops::Range<u64> = 0x100..0x200;

    match &error {
        TransportError::Connection(quinn::ConnectionError::ConnectionClosed(close))
            if ALERTS.contains(&u64::from(close.error_code)) =>
        {
            Error::Unpaired { device }
        }
        _ => Error::Transport(error),
    }
}

/// Why a session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The client's own runtime would not start.
    #[error("the client cannot start its own runtime")]
    Runtime(#[source] std::io::Error),
    /// Nothing answers to that name.
    #[error("{address} is not an address this client can reach")]
    Address {
        /// What the user typed.
        address: String,
    },
    /// This client has not paired with that device, so it holds no key to
    /// expect and no address to try.
    #[error("this client has not paired with {device}")]
    Unknown {
        /// The device that was asked for.
        device: DeviceId,
    },
    /// Neither the host's own addresses nor any relay led anywhere.
    #[error("cannot reach {device}: it answered at none of its addresses and no relay found it")]
    Unreachable {
        /// The device that was asked for.
        device: DeviceId,
    },
    /// The host answered and would not have this device.
    #[error("{device} answered but has not paired with this client: pair with it again")]
    Unpaired {
        /// The device that answered.
        device: DeviceId,
    },
    /// The list of paired hosts could not be read.
    #[error(transparent)]
    Hosts(#[from] CryptoError),
    /// A relay could not be reached, or would not do what was asked.
    #[error(transparent)]
    Relay(#[from] RelayError),
    /// The connection could not be made, or the host would not have it.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_is_sized_for_the_picture_the_client_asked_for() {
        // 1080 is coded as 1088, and a buffer sized for 1080 would be too small
        // for the key frame that actually arrives.
        let asked = Size::new(1920, 1080).expect("a size");
        assert!(
            tuning(asked).video_buffer
                >= Tuning::for_stream(CodedSize::new(1920, 1088).expect("codeable")).video_buffer
        );
    }
}
