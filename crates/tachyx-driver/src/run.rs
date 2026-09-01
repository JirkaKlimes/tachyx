//! The loop, and the one task beside it.
//!
//! Two tasks and no more. The reader exists because
//! [`ControlReader::recv`] reads a length and then the body it announces, so
//! cancelling it between the two would leave the stream cut in the wrong place.
//! Everything else is cancel-safe and belongs in one `select!`, where the
//! session is owned outright and needs no lock.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tachyx_client::Session;
use tachyx_proto::control::{ClientMessage, HostMessage};
use tachyx_transport::{Client, ClientSession, ControlError, ControlReader, ControlWriter};
use tokio::sync::mpsc;
use tokio::time::sleep_until;

use crate::{Command, GOODBYE, Shared};

/// How long input is collected before a batch goes out.
///
/// A finger on a 120 Hz phone produces a sample every eight milliseconds, so
/// this is short enough that no batch ever holds two samples of one motion for
/// long, and long enough that a drag costs one packet a tick rather than one
/// packet a sample. The tick is armed by the first event and disarmed by the
/// batch, so a session nobody is touching costs no wake-ups.
const INPUT_TICK: Duration = Duration::from_millis(4);

/// Runs one session until the connection ends or the handle is dropped.
pub(crate) async fn session(
    mut session: Session,
    opened: ClientSession,
    // The endpoint outlives the connection it dialled, so it is held here rather
    // than in the handle, where a caller would have to know why it was there.
    _client: Client,
    shared: Arc<Shared>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let ClientSession {
        connection,
        to_host,
        from_host,
        ..
    } = opened;
    let (messages, mut from_reader) = mpsc::unbounded_channel();
    tokio::spawn(read_control(from_host, messages));

    let mut to_host = to_host;
    let mut flush_at: Option<Instant> = None;
    let ended = loop {
        let expiry = session.next_deadline();
        let ended = tokio::select! {
            datagram = connection.read_datagram() => match datagram {
                Ok(datagram) => {
                    let now = Instant::now();
                    match session.datagram(&datagram, now) {
                        Ok(Some(frame)) => {
                            if let Some(sink) = shared.state().sink.clone() {
                                sink.push(frame.data);
                            }
                        }
                        Ok(None) => {}
                        Err(malformed) => {
                            tracing::debug!(%malformed, "a datagram that is not a video packet");
                        }
                    }
                    None
                }
                Err(gone) => Some(gone.to_string()),
            },
            message = from_reader.recv() => match message {
                Some(Ok(message)) => {
                    session.host(message, Instant::now());
                    None
                }
                Some(Err(fault)) => Some(fault.to_string()),
                None => Some("the host stopped speaking".to_owned()),
            },
            command = commands.recv() => match command {
                Some(Command::Input(at, event)) => {
                    session.input(at, event);
                    flush_at.get_or_insert_with(|| Instant::now() + INPUT_TICK);
                    None
                }
                Some(Command::Viewport(capabilities)) => {
                    session.capabilities_changed(capabilities);
                    None
                }
                // The handle is gone, so the user has left.
                None => break None,
            },
            () = sleep_until(at(flush_at)), if flush_at.is_some() => {
                session.flush_input();
                flush_at = None;
                None
            },
            () = sleep_until(at(expiry)), if expiry.is_some() => {
                session.expire(Instant::now());
                None
            },
        };

        let written = write_out(&mut session, &mut to_host, &shared).await;
        publish(&shared, &session);
        if let Some(reason) = ended.or(written) {
            break Some(reason);
        }
    };

    if let Some(reason) = ended {
        shared.state().ended = Some(reason);
    }
    connection.close(GOODBYE, b"the client is leaving");
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
    state.stream = session.stream();
    state.beat = session.heartbeat();
    state.heard = session.heard_at();
    state.saw = session.saw_at();
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
