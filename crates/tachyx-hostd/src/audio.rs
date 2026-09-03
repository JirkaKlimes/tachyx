//! The host's own output: which sink it is, how loud, and whether it is muted.
//!
//! One connection to the sound server, on a thread of its own, because the
//! `PulseAudio` client library is a main loop and not a set of calls. The library
//! answers the pipewire-pulse server as readily as a real `PulseAudio` one, which
//! is what makes this the same code on either.
//!
//! Everything that talks to the server happens on that one thread. The
//! alternative, calling in from wherever an answer is wanted, means the
//! library's own lock held across a round trip from a task that has other work
//! to do, and a subscription callback that runs on the library's thread and
//! would then need a lock of its own to reach anything. Here the callback only
//! puts a note on the same queue every other request arrives by, and the thread
//! that reads that queue is the only thread that ever calls the server.
//!
//! Two things are wanted from the server and both are why this exists at all.
//! The name of the default sink, because the sound to stream is the monitor
//! source of that sink and naming a device by hand is how a host works on one
//! machine and on none. And its volume and mute, because the host's own knob is
//! the volume: the tap is before it, so the client applies what is reported
//! here as playback gain, and a knob turned on the host has to reach the client
//! without anybody asking.
//!
//! Nothing in here panics on a server that is not there. The library does panic
//! on a request made to a context that is not connected, which is why every
//! request checks the state under the library's own lock: the main loop cannot
//! change that state while the lock is held, so a state read there is a state
//! that still holds when the request is made.

use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use libpulse_binding as pulse;
use pulse::context::introspect::Introspector;
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::threaded::Mainloop;
use pulse::volume::{ChannelVolumes, Volume};
use tachyx_proto::control::VOLUME_NORM;
use tokio::sync::watch;

/// The name this host announces itself to the sound server under.
const CLIENT_NAME: &str = "tachyx";

/// How long the server has to answer one request.
///
/// A local socket answers in well under a millisecond. This is the bound on a
/// server that has accepted the connection and then stopped speaking, which
/// must end the request rather than the thread.
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the whole connection has to come up.
///
/// It covers the socket, the authentication cookie and the first two
/// introspection round trips.
const OPEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the host's own sound could not be reached or changed.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The client library would not allocate its main loop.
    #[error("the pulseaudio client library would not start")]
    NoMainloop,
    /// No sound server answered.
    ///
    /// Either none is running, or this user cannot reach the one that is.
    /// `PULSE_SERVER` and `XDG_RUNTIME_DIR` are where it would have looked.
    #[error("no sound server answered: {0}")]
    NoServer(String),
    /// The server answered and named no default sink, which is a machine with
    /// no output device.
    #[error("the sound server has no default sink, so there is no output to stream")]
    NoDefaultSink,
    /// The server has a default sink by that name and would not describe it.
    #[error("the sound server does not describe its own default sink {0}")]
    NoSuchSink(String),
    /// The server took the change and would not make it.
    #[error("the sound server refused the change")]
    Refused,
    /// The connection was accepted and then went quiet.
    #[error("the sound server did not answer within {REPLY_TIMEOUT:?}")]
    TimedOut,
    /// The connection is gone. A session in this state streams nothing rather
    /// than ending.
    #[error("the connection to the sound server is gone")]
    Disconnected,
    /// GStreamer has none of the elements the pipeline is built from.
    #[error(
        "gstreamer offers no pulsesrc or opusenc: the plugins are missing from this environment \
         (run inside `nix develop`)"
    )]
    NoElements,
    /// The GStreamer registry could not be read at all.
    #[error(transparent)]
    Media(#[from] tachyx_media::Error),
    /// The thread that owns the connection could not be started.
    #[error("cannot start the sound control thread")]
    Thread(#[from] std::io::Error),
}

/// The host's default output, as the sound server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    /// The sink's own name, which nothing but the server chooses.
    pub name: String,
    /// Its volume, in permille of [`VOLUME_NORM`].
    pub volume: u16,
    /// Whether it is muted.
    pub muted: bool,
}

impl Sink {
    /// The source this sink's own output can be recorded from.
    ///
    /// Every sink has one, named after it. It carries what the sink plays
    /// before the sink's volume and mute are applied, which is what lets the
    /// host be muted while the client still hears everything.
    #[must_use]
    pub fn monitor(&self) -> String {
        format!("{}.monitor", self.name)
    }
}

/// Whether this host can stream its own sound, and why not when it cannot.
///
/// Both halves of the answer. GStreamer has to offer the elements the pipeline
/// is built from, and a sound server has to answer with an output to record. A
/// host announces sound to a client only when both said yes, because a control
/// that does nothing is worse than no control at all.
///
/// It opens a connection and closes it again, so it is blocking. Run it off a
/// runtime worker.
///
/// # Errors
/// Whichever half said no.
pub fn probe() -> Result<Sink, AudioError> {
    if !tachyx_media::audio_available()? {
        return Err(AudioError::NoElements);
    }
    Ok(HostAudio::open()?.sink())
}

/// What the thread that owns the connection is asked to do.
#[derive(Debug, Clone, Copy)]
enum Command {
    /// The server said something changed. Read it again.
    Reread,
    /// Set the default sink's volume, in permille of [`VOLUME_NORM`].
    Volume(u16),
    /// Set the default sink's mute switch.
    Muted(bool),
    /// Finish.
    Stop,
}

/// A live connection to the sound server.
///
/// Cheap to hold and safe to drop: dropping it ends the thread and the
/// connection with it.
#[derive(Debug)]
pub struct HostAudio {
    commands: Sender<Command>,
    sink: watch::Receiver<Sink>,
    thread: Option<JoinHandle<()>>,
}

impl HostAudio {
    /// Connects to the sound server and reads its default sink.
    ///
    /// Blocking, and bounded by [`OPEN_TIMEOUT`]. Call it off a runtime worker.
    ///
    /// # Errors
    /// If no server answers, it names no default sink, or it does not answer in
    /// time.
    pub fn open() -> Result<Self, AudioError> {
        let (commands, requests) = channel();
        let (started, ready) = sync_channel(1);
        let notify = commands.clone();
        let thread = std::thread::Builder::new()
            .name("tachyx-pulse".to_owned())
            .spawn(move || run(&started, &requests, &notify))?;
        let sink = ready
            .recv_timeout(OPEN_TIMEOUT)
            .map_err(|_| AudioError::TimedOut)??;
        Ok(Self {
            commands,
            sink,
            thread: Some(thread),
        })
    }

    /// The default sink as it was last reported.
    #[must_use]
    pub fn sink(&self) -> Sink {
        self.sink.borrow().clone()
    }

    /// A view of the default sink that wakes whenever it changes.
    ///
    /// This is how a knob turned on the host reaches the client. The server
    /// tells us, we tell whoever holds one of these, and nobody polls.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<Sink> {
        self.sink.clone()
    }

    /// Sets the default sink's volume, in permille of [`VOLUME_NORM`].
    ///
    /// It returns before the server has applied it. The answer comes back as a
    /// change on [`HostAudio::changes`], which is the same path a knob turned
    /// on the host arrives by, so there is one way volume is ever learned.
    pub fn set_volume(&self, permille: u16) {
        self.ask(Command::Volume(permille.min(VOLUME_NORM)));
    }

    /// Sets the default sink's mute switch.
    pub fn set_muted(&self, muted: bool) {
        self.ask(Command::Muted(muted));
    }

    /// Mutes the host's own output until the guard is dropped.
    ///
    /// What it restores is the mute the host had before, whatever that was, so
    /// a host that was already muted stays muted afterwards. The restore has to
    /// happen however the session ends, which is why it is a guard and not a
    /// pair of calls: a client that disconnects mid-session must not leave the
    /// machine silent.
    #[must_use]
    pub fn silence(&self) -> Silenced {
        let restore = self.sink.borrow().muted;
        self.set_muted(true);
        Silenced {
            commands: self.commands.clone(),
            restore,
        }
    }

    fn ask(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::debug!(?command, "the connection to the sound server has ended");
        }
    }
}

impl Drop for HostAudio {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the sound control thread panicked");
        }
    }
}

/// Holds the host's own output muted, and unmutes it on the way out.
#[derive(Debug)]
pub struct Silenced {
    commands: Sender<Command>,
    restore: bool,
}

impl Drop for Silenced {
    fn drop(&mut self) {
        if self.commands.send(Command::Muted(self.restore)).is_err() {
            tracing::warn!(
                "the host was left muted: the connection to the sound server ended before it \
                 could be restored"
            );
        }
    }
}

/// Owns the connection for as long as it lasts.
///
/// It reports how it started exactly once, and then does nothing but serve the
/// queue. Every request in the queue is one round trip, and the subscription
/// puts its own notes on the same queue, so there is one order of events and
/// one thread applying them.
fn run(
    started: &SyncSender<Result<watch::Receiver<Sink>, AudioError>>,
    requests: &Receiver<Command>,
    notify: &Sender<Command>,
) {
    let Some(mut mainloop) = Mainloop::new() else {
        let _ = started.send(Err(AudioError::NoMainloop));
        return;
    };
    let Some(mut context) = Context::new(&mainloop, CLIENT_NAME) else {
        let _ = started.send(Err(AudioError::NoMainloop));
        return;
    };
    let sink = match connect(&mut mainloop, &mut context).and_then(|()| read(&mut mainloop, &context))
    {
        Ok(sink) => sink,
        Err(error) => {
            let _ = started.send(Err(error));
            mainloop.stop();
            return;
        }
    };

    let (changes, watching) = watch::channel(sink);
    if started.send(Ok(watching)).is_err() {
        mainloop.stop();
        return;
    }
    subscribe(&mut mainloop, &mut context, notify.clone());

    for command in requests {
        let outcome = match command {
            Command::Stop => break,
            Command::Reread => Ok(()),
            Command::Volume(permille) => volume(&mut mainloop, &context, permille),
            Command::Muted(muted) => mute(&mut mainloop, &context, muted),
        };
        if let Err(error) = outcome {
            tracing::debug!(%error, ?command, "the sound server refused a change");
        }
        // Read back rather than assumed, on every turn. The server is free to
        // clamp what it was asked for, and it is also free to change on its
        // own, so what is published is always what it says now.
        match read(&mut mainloop, &context) {
            Ok(sink) if *changes.borrow() != sink => {
                changes.send_replace(sink);
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "cannot read the host's own output"),
        }
    }
    // Stopped before either object is dropped, and without the lock held, so
    // that no callback is running inside anything being freed.
    mainloop.stop();
}

/// Brings the connection up, or says why it did not.
fn connect(mainloop: &mut Mainloop, context: &mut Context) -> Result<(), AudioError> {
    let (woken, wake) = sync_channel(8);
    context.set_state_callback(Some(Box::new(move || {
        let _ = woken.try_send(());
    })));
    // No autospawn. A daemon that starts a sound server of its own is a daemon
    // that has taken over the session's audio, and a host with no server is a
    // host that streams no sound, which is a thing to report rather than fix.
    context
        .connect(None, FlagSet::NOAUTOSPAWN, None)
        .map_err(refused)?;
    mainloop.start().map_err(refused)?;

    let deadline = std::time::Instant::now() + OPEN_TIMEOUT;
    loop {
        mainloop.lock();
        let state = context.get_state();
        mainloop.unlock();
        match state {
            State::Ready => return Ok(()),
            State::Failed | State::Terminated => {
                return Err(AudioError::NoServer("the connection was refused".to_owned()));
            }
            _ => {}
        }
        let left = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or(AudioError::TimedOut)?;
        // A wake that arrives is a state change to look at; one that does not
        // is the deadline, checked on the way round.
        let _ = wake.recv_timeout(left);
    }
}

/// The library's own word for why, which is a number until `pa_strerror` is
/// asked to name it.
fn refused(error: pulse::error::PAErr) -> AudioError {
    AudioError::NoServer(
        error
            .to_string()
            .unwrap_or_else(|| format!("error {}", error.0)),
    )
}

/// Asks to be told when the default sink, or which sink is the default,
/// changes.
///
/// The callback runs on the library's own thread, so it does the least it can:
/// it puts a note on the queue the owning thread already serves, and every
/// request to the server is still made from that one thread.
fn subscribe(mainloop: &mut Mainloop, context: &mut Context, notify: Sender<Command>) {
    context.set_subscribe_callback(Some(Box::new(move |_, _, _| {
        let _ = notify.send(Command::Reread);
    })));
    mainloop.lock();
    if context.get_state() == State::Ready {
        // The sink for its volume and mute, and the server because the default
        // sink is a property of the server rather than of any sink: plugging in
        // headphones moves it, and the monitor to record then moves with it.
        context.subscribe(InterestMaskSet::SINK | InterestMaskSet::SERVER, |_| {});
    }
    mainloop.unlock();
}

/// Which sink the server is playing everything through right now.
///
/// Asked again every time rather than remembered, because it moves: plugging in
/// headphones makes them the default, and the monitor source to record moves
/// with it.
fn default_sink(mainloop: &mut Mainloop, context: &Context) -> Result<String, AudioError> {
    ask(mainloop, context, |introspect, answer| {
        introspect.get_server_info(move |info| {
            let _ = answer.try_send(info.default_sink_name.as_ref().map(ToString::to_string));
        });
    })?
    .ok_or(AudioError::NoDefaultSink)
}

/// Reads the default sink and what it is set to.
fn read(mainloop: &mut Mainloop, context: &Context) -> Result<Sink, AudioError> {
    let name = default_sink(mainloop, context)?;
    let (volume, muted) = state_of(mainloop, context, &name)?;
    Ok(Sink {
        name,
        volume: permille(&volume),
        muted,
    })
}

/// The volume and mute of one sink, by name.
fn state_of(
    mainloop: &mut Mainloop,
    context: &Context,
    name: &str,
) -> Result<(ChannelVolumes, bool), AudioError> {
    let asked = name.to_owned();
    // The list callback runs once per sink and once more to say the list has
    // ended, so the first answer is the sink and a bare ending means there was
    // none.
    ask(mainloop, context, move |introspect, answer| {
        introspect.get_sink_info_by_name(&asked, move |result| {
            let _ = answer.try_send(match result {
                pulse::callbacks::ListResult::Item(info) => Some((info.volume, info.mute)),
                pulse::callbacks::ListResult::End | pulse::callbacks::ListResult::Error => None,
            });
        });
    })?
    .ok_or_else(|| AudioError::NoSuchSink(name.to_owned()))
}

/// Sets the default sink's volume, keeping its channel layout.
///
/// The layout has to be read first. A volume is one value per channel, and
/// writing a two-channel volume onto a sink with more channels silences the
/// rest.
fn volume(mainloop: &mut Mainloop, context: &Context, wanted: u16) -> Result<(), AudioError> {
    let name = default_sink(mainloop, context)?;
    let (mut volumes, _) = state_of(mainloop, context, &name)?;
    volumes.set(volumes.len(), raw(wanted));
    apply(mainloop, context, move |introspect, answer| {
        introspect.set_sink_volume_by_name(
            &name,
            &volumes,
            Some(Box::new(move |done| {
                let _ = answer.try_send(done);
            })),
        );
    })
}

/// Sets the default sink's mute switch.
fn mute(mainloop: &mut Mainloop, context: &Context, muted: bool) -> Result<(), AudioError> {
    let name = default_sink(mainloop, context)?;
    apply(mainloop, context, move |introspect, answer| {
        introspect.set_sink_mute_by_name(
            &name,
            muted,
            Some(Box::new(move |done| {
                let _ = answer.try_send(done);
            })),
        );
    })
}

/// Runs one change and waits for the server to say it took.
fn apply(
    mainloop: &mut Mainloop,
    context: &Context,
    call: impl FnOnce(&mut Introspector, SyncSender<bool>),
) -> Result<(), AudioError> {
    if ask(mainloop, context, call)? {
        return Ok(());
    }
    Err(AudioError::Refused)
}

/// Makes one request and waits for its answer.
///
/// The library's lock is held for the request and released for the answer, and
/// both halves of that matter. Held, because a request made on a context that
/// is not connected panics inside the library rather than failing, and the main
/// loop cannot move the state while the lock is held, so the state read here is
/// the state the request is made under. Released, because the answer is
/// delivered by that same main loop, which cannot run while the lock is held.
fn ask<T: Send + 'static>(
    mainloop: &mut Mainloop,
    context: &Context,
    call: impl FnOnce(&mut Introspector, SyncSender<T>),
) -> Result<T, AudioError> {
    // Two deep: a list answers with an item and then with an end, and neither
    // may block the main loop's own thread.
    let (answer, reply) = sync_channel(2);
    mainloop.lock();
    let ready = context.get_state() == State::Ready;
    if ready {
        call(&mut context.introspect(), answer);
    }
    mainloop.unlock();
    if !ready {
        return Err(AudioError::Disconnected);
    }
    reply
        .recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| AudioError::TimedOut)
}

/// A sink volume as permille of [`VOLUME_NORM`].
///
/// The average across channels, which is what a single knob means, and clamped
/// at unity because the sound server allows more than unity and the wire does
/// not. A host set above a hundred per cent therefore reports a hundred, and a
/// client that then writes a hundred back lowers it. That is the cost of one
/// knob with one meaning at both ends.
fn permille(volume: &ChannelVolumes) -> u16 {
    let unity = u64::from(Volume::NORMAL.0);
    // Rounded rather than truncated, and that is not a nicety. Unity is 65536
    // and permille is a thousandth of it, so the two scales do not divide: 999
    // permille is written as 65470 and reads back as 998. Setting a volume and
    // reading it again has to give the same number, or a client that shows what
    // it reads shows a knob that slips by one every time it is touched.
    let raw = u64::from(volume.avg().0) * u64::from(VOLUME_NORM) + unity / 2;
    u16::try_from(raw / unity)
        .unwrap_or(VOLUME_NORM)
        .min(VOLUME_NORM)
}

/// The reverse: permille of [`VOLUME_NORM`] as a sink volume.
fn raw(permille: u16) -> Volume {
    Volume(u32::from(permille.min(VOLUME_NORM)) * Volume::NORMAL.0 / u32::from(VOLUME_NORM))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_ends_of_the_volume_scale_agree() {
        let mut volumes = ChannelVolumes::default();
        for wanted in [0u16, 1, 3, 250, 500, 999, VOLUME_NORM] {
            volumes.set(2, raw(wanted));
            assert_eq!(
                permille(&volumes),
                wanted,
                "{wanted} permille did not survive the round trip"
            );
        }
    }

    /// The sound server allows more than unity and the wire carries permille of
    /// unity, so anything above it reports as unity rather than wrapping.
    #[test]
    fn a_sink_turned_past_unity_reports_unity() {
        let mut volumes = ChannelVolumes::default();
        volumes.set(2, Volume(Volume::NORMAL.0 * 3 / 2));
        assert_eq!(permille(&volumes), VOLUME_NORM);
        assert_eq!(raw(VOLUME_NORM + 500), Volume::NORMAL);
    }

    #[test]
    fn a_monitor_is_named_after_the_sink_it_records() {
        let sink = Sink {
            name: "alsa_output.pci-0000_00_1f.3.analog-stereo".to_owned(),
            volume: VOLUME_NORM,
            muted: false,
        };
        assert_eq!(
            sink.monitor(),
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        );
    }
}
