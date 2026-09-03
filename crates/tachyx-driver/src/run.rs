//! The loop, and the one task beside it.
//!
//! Two tasks and no more. The reader exists because
//! [`ControlReader::recv`] reads a length and then the body it announces, so
//! cancelling it between the two would leave the stream cut in the wrong place.
//! Everything else is cancel-safe and belongs in one `select!`, where the
//! session is owned outright and needs no lock.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tachyx_client::{AudioEvent, Liveness, Session};
use tachyx_proto::control::{ClientMessage, HostMessage};
use tachyx_transport::{ClientSession, ControlError, ControlReader, ControlWriter};
use tokio::sync::mpsc;
use tokio::time::sleep_until;

use crate::{Command, GOODBYE, Held, Shared};

/// The shortest time between two input batches.
///
/// A finger on a 120 Hz phone produces a sample every eight milliseconds, so
/// this is short enough that no batch ever holds two samples of one motion for
/// long, and long enough that a mouse reporting a thousand times a second costs
/// one packet a tick rather than one packet a sample. See [`Ticker`].
const INPUT_TICK: Duration = Duration::from_millis(4);

/// How long the loop may go without copying what it knows to the handle.
///
/// Everything that reads it draws: the statistics panel at the display's rate,
/// the pointer once a frame. A twenty megabit stream lands a datagram every
/// half millisecond, so publishing after each one takes the lock the drawing
/// thread wants some seventeen hundred times a second, to move numbers that are
/// read sixty. Half a frame of the fastest display anyone streams to is as
/// fresh as a reader can use, and it costs at most a quarter of the locks.
const PUBLISH_TICK: Duration = Duration::from_millis(4);

/// Runs one session until the connection ends or the handle is dropped.
pub(crate) async fn session(
    mut session: Session,
    opened: ClientSession,
    // What the connection runs on: the endpoint, and the relay connection when
    // there is one. Both outlive the connection, and both are held here rather
    // than in the handle, where a caller would have to know why they were there.
    _held: Held,
    shared: Arc<Shared>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let ClientSession {
        connection,
        greeting,
        to_host,
        from_host,
        ..
    } = opened;
    // What the host said about itself before it had read a word from this
    // client. Only the session may act on it, and only it knows what for.
    session.greeted(greeting);
    let (messages, mut from_reader) = mpsc::unbounded_channel();
    tokio::spawn(read_control(from_host, messages));

    let mut to_host = to_host;
    let mut input = Ticker::default();
    let mut published = Instant::now();
    // Whether a complete picture has been thrown away for want of a decoder.
    let mut unshown = false;
    let mut reports = tokio::time::interval(REPORT_EVERY);
    let ended = loop {
        let expiry = session.next_deadline();
        let ended = tokio::select! {
            datagram = connection.read_datagram() => match datagram {
                Ok(datagram) => {
                    if let Err(malformed) = session.datagram(&datagram, Instant::now()) {
                        tracing::debug!(%malformed, "a datagram that is not a media packet");
                    }
                    present(&mut session, &shared, &mut unshown);
                    listen(&mut session, &shared);
                    None
                }
                Err(gone) => Some(gone.to_string()),
            },
            message = from_reader.recv() => match message {
                Some(Ok(message)) => {
                    session.host(message, Instant::now(), connection.rtt());
                    None
                }
                Some(Err(fault)) => Some(fault.to_string()),
                None => Some("the host stopped speaking".to_owned()),
            },
            command = commands.recv() => match command {
                Some(command) => {
                    asked(command, &mut session, &shared, &mut input, &mut unshown);
                    None
                }
                // The handle is gone, so the user has left.
                None => break None,
            },
            () = sleep_until(at(input.due)), if input.due.is_some() => {
                session.flush_input();
                input.close(Instant::now());
                None
            },
            () = sleep_until(at(expiry)), if expiry.is_some() => {
                session.expire(Instant::now());
                present(&mut session, &shared, &mut unshown);
                None
            },
            _ = reports.tick() => {
                report(&session, connection.rtt(), &shared);
                None
            },
        };

        let written = write_out(&mut session, &mut to_host, &shared).await;
        let over = ended.or(written);
        let now = Instant::now();
        // Whoever reads this draws, so copying faster than a frame is copying
        // into a lock nobody looks at. The last word of a session is not a
        // redraw and does not wait for one.
        if over.is_some() || now >= published + PUBLISH_TICK {
            publish(&shared, &session);
            published = now;
        }
        if let Some(reason) = over {
            break Some(reason);
        }
    };

    if let Some(reason) = ended {
        shared.state().ended = Some(reason);
    }
    connection.close(GOODBYE, b"the client is leaving");
}

/// How often the loop says out loud what it has seen.
///
/// A device has no terminal, so what reassembly saw is only visible if it is
/// said at a steady cadence. Half a second is often enough to watch a link
/// collapse and rare enough to read.
const REPORT_EVERY: Duration = Duration::from_millis(500);

/// Writes one line about what the video path has done since the session opened.
fn report(session: &Session, round_trip: Duration, shared: &Arc<Shared>) {
    let stats = session.video_stats();
    let live = Liveness::at(Instant::now(), session.heard_at(), session.saw_at());
    let (host_audio, audio_packets, audio_lost, audio) = {
        let state = shared.state();
        (state.host_audio, state.audio_packets, state.audio_lost, state.audio)
    };
    tracing::info!(
        host_audio,
        audio_packets,
        audio_lost,
        audio_mode = ?audio.map(|a| a.mode),
        ?live,
        recv = stats.packets_received,
        lost = stats.packets_lost,
        late = stats.packets_late,
        recovered = stats.frames_recovered,
        delivered = stats.frames_delivered,
        dropped = stats.frames_dropped,
        undecodable = stats.frames_undecodable,
        rtt_ms = u64::try_from(round_trip.as_millis()).unwrap_or(u64::MAX),
        "client",
    );
}

/// When input last went out, and when what is waiting is due.
///
/// Two batches never come closer together than [`INPUT_TICK`], which is what
/// holds a drag to one packet a tick however fast the device samples. What the
/// tick must not do is make the first event of a quiet period wait for a batch
/// that nothing will join: a mouse at rest sends exactly that, and the wait is
/// four milliseconds of latency bought for nothing. So a batch that is already
/// a tick clear of the last one goes out at once, and only a batch that would
/// crowd it waits. Both cost no wake-up while nobody is touching the session.
#[derive(Debug, Default)]
struct Ticker {
    /// When the last batch went out, if one has.
    sent: Option<Instant>,
    /// When the batch now collecting is due on the wire.
    due: Option<Instant>,
}

impl Ticker {
    /// Opens a batch for an event that arrived with nothing queued behind it,
    /// and says whether that batch goes out now.
    ///
    /// A batch that does not is made due a tick after the last one, so the rate
    /// on the wire is the same either way.
    fn open(&mut self, now: Instant) -> bool {
        match self.sent {
            Some(last) if now < last + INPUT_TICK => {
                self.due = Some(last + INPUT_TICK);
                false
            }
            _ => true,
        }
    }

    /// Notes the batch that was waiting as gone.
    fn close(&mut self, now: Instant) {
        self.sent = Some(now);
        self.due = None;
    }
}

/// Does what the handle asked for.
///
/// Everything here changes the session or what it publishes and none of it can
/// end the session, which is why the loop above keeps only the one case that
/// can: the handle itself going away.
fn asked(
    command: Command,
    session: &mut Session,
    shared: &Arc<Shared>,
    input: &mut Ticker,
    unshown: &mut bool,
) {
    match command {
        Command::Input(at, event) => {
            let now = Instant::now();
            // Asked before the push, because what decides this is whether the
            // event has a batch to join, not whether it made one.
            let alone = !session.input_pending();
            session.input(at, now, event);
            // The pointer this just moved, said at once rather than on the
            // publish tick. Whoever draws it samples on the display's clock, and
            // a position republished on a clock of its own beats against that
            // one: the drawn pointer takes a long step on one frame and a short
            // one on the next, which is a drag that stutters however steady the
            // finger was. One lock per touch sample, not one per datagram, so
            // the reason PUBLISH_TICK exists is untouched.
            shared.state().cursor = session.cursor();
            if alone && input.open(now) {
                session.flush_input();
                input.close(now);
            }
        }
        Command::Viewport(capabilities) => session.capabilities_changed(capabilities),
        Command::Present(sink) => {
            shared.state().sink = Some(sink);
            if std::mem::take(unshown) {
                session.decoder_started_late(Instant::now());
            }
        }
        Command::Listen(speaker) => shared.state().speaker = Some(speaker),
        Command::Audio(command) => session.audio_command(command),
    }
}

/// Hands every picture the session has finished to the decoder.
///
/// One datagram can finish more than one: the datagram that completes a frame
/// is also what decides the damaged frames below it, and any of those the
/// parity rebuilt come out ahead of it.
fn present(session: &mut Session, shared: &Arc<Shared>, unshown: &mut bool) {
    let mut sink = None;
    while let Some(frame) = session.pop_frame() {
        // Read once a batch, and only where there is a picture to present:
        // most datagrams finish none, and this is the lock the thread that
        // draws the application reads. Taken out of it before it is used,
        // because handing a frame to a decoder takes as long as the decoder
        // takes.
        match sink.get_or_insert_with(|| shared.state().sink.clone()) {
            Some(sink) => sink.push(frame.data),
            // Nothing is presenting yet, so this picture is gone and every
            // picture after it refers back to one no decoder has seen. Only
            // this loop knows: reassembly saw the picture arrive whole.
            None => *unshown = true,
        }
    }
}

/// Hands everything the session has decided about sound to the speakers.
///
/// The counters are the loop's own and are kept whether or not anything is
/// playing, so a session that lost its sound device still says how much sound
/// arrived. The sink is read once a batch and only when there is sound, for the
/// reason [`present`] gives: this is the lock the drawing thread wants.
fn listen(session: &mut Session, shared: &Arc<Shared>) {
    let mut speaker = None;
    let mut packets = 0;
    let mut lost = 0;
    while let Some(event) = session.pop_audio() {
        match &event {
            AudioEvent::Packet { .. } => packets += 1,
            AudioEvent::Lost(missing) => lost += u64::from(*missing),
        }
        // Nothing is playing yet, so this sound is gone. It is a moment of a
        // live stream: held back and played later it would be behind the
        // picture for the rest of the session.
        if let Some(speaker) = speaker.get_or_insert_with(|| shared.state().speaker.clone()) {
            speaker.push(event);
        }
    }
    if packets > 0 || lost > 0 {
        let mut state = shared.state();
        state.audio_packets += packets;
        state.audio_lost += lost;
    }
}

/// Writes everything the session has to say, reporting the first refusal.
async fn write_out(
    session: &mut Session,
    to_host: &mut ControlWriter,
    shared: &Arc<Shared>,
) -> Option<String> {
    while let Some(message) = session.poll_control() {
        let batch = matches!(message, ClientMessage::Input(_));
        if let Err(fault) = to_host.send(&message).await {
            return Some(fault.to_string());
        }
        if batch {
            shared.state().batches += 1;
        }
    }
    None
}

/// Copies what the loop knows to where the handle can read it.
fn publish(shared: &Arc<Shared>, session: &Session) {
    let mut state = shared.state();
    state.video = session.video_stats();
    state.host_audio = session.host_has_audio();
    state.audio = session.audio_status();
    state.stream = session.stream();
    state.beat = session.heartbeat();
    state.heard = session.heard_at();
    state.saw = session.saw_at();
    state.cursor = session.cursor();
}

/// Reads the control stream into a channel the loop can select on.
///
/// A message is a length and then the body it announces, so this cannot be a
/// `select!` arm: cancelling it between the two reads would leave the stream cut
/// in the wrong place and every later message misparsed.
async fn read_control(
    mut from_host: ControlReader,
    to_loop: mpsc::UnboundedSender<Result<HostMessage, ControlError>>,
) {
    loop {
        let message = from_host.recv::<HostMessage>().await;
        let ended = message.is_err();
        if to_loop.send(message).is_err() || ended {
            return;
        }
    }
}

/// The runtime's own clock reading for a deadline the select is guarded on.
///
/// Only reached on an enabled branch, and every enabled branch has one.
fn at(deadline: Option<Instant>) -> tokio::time::Instant {
    deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std)
}

#[cfg(test)]
mod tests {
    use super::{Ticker, INPUT_TICK};
    use std::time::{Duration, Instant};

    #[test]
    fn the_first_event_of_a_quiet_period_does_not_wait() {
        let mut ticker = Ticker::default();
        let start = Instant::now();
        assert!(ticker.open(start), "nothing has gone out, so nothing waits");
        ticker.close(start);
        assert!(
            ticker.open(start + INPUT_TICK),
            "a tick has passed, so the batch is alone and goes now"
        );
    }

    #[test]
    fn an_event_that_would_crowd_the_last_batch_waits_out_the_tick() {
        let mut ticker = Ticker::default();
        let start = Instant::now();
        ticker.close(start);
        assert!(!ticker.open(start + Duration::from_millis(1)));
        assert_eq!(
            ticker.due,
            Some(start + INPUT_TICK),
            "due a tick after the last batch, not a tick after the event"
        );
    }

    #[test]
    fn a_device_sampling_faster_than_the_tick_still_costs_one_batch_a_tick() {
        // A gaming mouse at a thousand hertz, for a tenth of a second.
        let start = Instant::now();
        let mut ticker = Ticker::default();
        let mut sent = Vec::new();
        let mut waiting = false;
        for step in 0..100 {
            let now = start + Duration::from_millis(step);
            if waiting && ticker.due.is_some_and(|due| due <= now) {
                ticker.close(now);
                sent.push(now);
                waiting = false;
            }
            if waiting {
                continue;
            }
            if ticker.open(now) {
                ticker.close(now);
                sent.push(now);
            } else {
                waiting = true;
            }
        }
        assert!(
            sent.windows(2).all(|pair| pair[1] >= pair[0] + INPUT_TICK),
            "no two batches came closer than a tick: {sent:?}"
        );
        assert!(
            sent.len() <= 100 / 4 + 1,
            "a hundred milliseconds holds twenty five ticks, and got {}",
            sent.len()
        );
    }
}
