//! The client's I/O driver.
//!
//! `tachyx-client` decides what a session does and touches nothing;
//! `tachyx-transport` moves bytes and decides nothing. This is the loop between
//! them: it dials a host, hands the session the messages that arrive, writes the
//! messages the session produces, and gives complete pictures to a decoder.
//!
//! It is the only place in the client that owns a clock, a socket and a
//! runtime, which is what keeps the two crates under it testable without any of
//! the three.
//!
//! The decoder is reached through [`FrameSink`] rather than named directly.
//! Linux decodes through GStreamer into a Flutter texture and Android through
//! the platform decoder into a surface producer; neither belongs in a loop whose
//! whole job is protocol.

#![forbid(unsafe_code)]

mod run;

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use quinn::{Connection, VarInt};
use tachyx_client::{CursorView, Liveness, Session};
use tachyx_proto::control::{Capabilities, CodedSize, Heartbeat, Size, StreamConfig};
use tachyx_proto::input::InputEvent;
use tachyx_proto::media::Timestamp;
use tachyx_proto::packet::ReassemblyStats;
use tachyx_transport::{Client, PinStore, TransportError, Tuning};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

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
    batches: u64,
    stream: Option<StreamConfig>,
    beat: Option<Heartbeat>,
    heard: Instant,
    saw: Option<Instant>,
    ended: Option<String>,
    sink: Option<Arc<dyn FrameSink>>,
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
                batches: 0,
                stream: None,
                beat: None,
                heard: now,
                saw: None,
                ended: None,
                sink: None,
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
    /// Dials `address`, introduces the client, and runs the session.
    ///
    /// `address` is `host:port`, where the host is a name or a literal address.
    /// It is also the identity the host's key is pinned under, so the same host
    /// reached by a different name is a different host as far as the pin is
    /// concerned. `state` is the directory those pins are kept in.
    ///
    /// # Errors
    /// If the address names nothing, the runtime will not start, the host
    /// presents a key other than the pinned one, or the host refuses the
    /// session. A refusal carries the host's own words.
    pub fn connect(address: &str, state: &Path, capabilities: Capabilities) -> Result<Self, Error> {
        let runtime = Builder::new_multi_thread()
            // One for quinn's endpoint driver and one for the session loop. A
            // client runs exactly two tasks and a phone has better uses for its
            // cores.
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(Error::Runtime)?;

        let name = host_of(address).to_owned();
        let now = Instant::now();
        let session = Session::new(capabilities, now);
        // All of it inside the runtime: a quinn endpoint binds its socket to
        // whichever reactor is current, and outside one there is none to bind
        // to.
        let (client, opened) = runtime.block_on(async {
            let socket = resolve(address).await?;
            let pins = Arc::new(PinStore::open(state.join("pins")).map_err(TransportError::from)?);
            let client = Client::new(pins, tuning(capabilities.max_decode))?;
            let opened = client.connect(socket, &name, &session.hello()).await?;
            Ok::<_, Error>((client, opened))
        })?;
        let connection = opened.connection.clone();

        let shared = Arc::new(Shared::new(now));
        let (commands, from_handle) = mpsc::unbounded_channel();
        runtime.spawn(run::session(
            session,
            opened,
            client,
            Arc::clone(&shared),
            from_handle,
        ));

        Ok(Self {
            connection,
            commands,
            shared,
            runtime: Some(runtime),
        })
    }

    /// What this session has done. See [`Stats`].
    #[must_use]
    pub fn stats(&self) -> Stats {
        let state = self.shared.state();
        Stats {
            liveness: Liveness::at(Instant::now(), state.heard, state.saw),
            rtt: self.connection.rtt(),
            video: state.video,
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

/// The host part of a `host:port`, with the brackets an IPv6 literal wears
/// taken off.
fn host_of(address: &str) -> &str {
    address
        .rsplit_once(':')
        .map_or(address, |(host, _)| host)
        .trim_matches(['[', ']'])
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
    /// The connection could not be made, or the host would not have it.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_filed_under_the_host_and_not_the_port() {
        assert_eq!(host_of("10.0.0.4:4433"), "10.0.0.4");
        assert_eq!(host_of("desk.local:4433"), "desk.local");
        assert_eq!(host_of("[fe80::1]:4433"), "fe80::1");
        assert_eq!(host_of("desk.local"), "desk.local");
    }

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
