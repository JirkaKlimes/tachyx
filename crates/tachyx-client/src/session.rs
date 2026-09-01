//! The session, as a state machine that moves bytes rather than sockets.
//!
//! [`Session`] performs no I/O. It is handed the bytes that arrived on the
//! control stream and asked for the bytes to write back, so the whole of the
//! client's protocol behaviour is exercised by a test with no network, no
//! runtime and no timing. The QUIC connection that carries those bytes belongs
//! to `tachyx-transport`.

use tachyx_proto::input::InputEvent;

use crate::control::{
    ClientHello, ClientMessage, ClientTime, ControlError, HostHello, HostMessage, MessageStream,
    StreamConfig, Viewport, encode,
};
use crate::input::InputQueue;

/// How far the session has got.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// The hello has been queued and the host has not answered yet.
    Greeting,
    /// The host answered and has not yet said what it will send.
    Greeted(HostHello),
    /// The host is streaming under this configuration.
    Streaming {
        /// What the host said about itself.
        host: HostHello,
        /// What the host is sending.
        stream: StreamConfig,
    },
}

impl Phase {
    /// What the host is currently sending, if it has said.
    #[must_use]
    pub const fn stream(&self) -> Option<&StreamConfig> {
        match self {
            Self::Streaming { stream, .. } => Some(stream),
            Self::Greeting | Self::Greeted(_) => None,
        }
    }
}

/// Something the user interface has to react to.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The host introduced itself.
    Greeted(HostHello),
    /// The host will send this from now on. Arrives again on every change,
    /// including one forced by congestion, and the decoder is reconfigured on it.
    Stream(StreamConfig),
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The host speaks a different version of the protocol. Reported rather than
    /// misparsed, because `postcard` is not self-describing and would otherwise
    /// read one message as another.
    #[error("host speaks protocol version {host}; this client speaks {ours}")]
    VersionMismatch {
        /// The version the host announced.
        host: u16,
        /// The version this client speaks.
        ours: u16,
    },
    /// The host said something before it introduced itself.
    #[error("host sent a message before its hello")]
    NoGreeting,
    /// The host introduced itself twice.
    #[error("host sent a second hello")]
    GreetedTwice,
    /// The control stream carried something that is not a message.
    #[error(transparent)]
    Control(#[from] ControlError),
}

/// A client session.
#[derive(Debug)]
pub struct Session {
    phase: Phase,
    incoming: MessageStream,
    outgoing: Vec<Vec<u8>>,
    input: InputQueue,
}

impl Session {
    /// Opens a session that will introduce itself with `hello`.
    ///
    /// The hello is queued immediately, so the first [`Session::poll_control`]
    /// hands back the bytes to write on the freshly opened stream.
    ///
    /// # Errors
    /// If the hello does not fit a control message, which takes an implausible
    /// list of decoders.
    pub fn new(hello: ClientHello) -> Result<Self, ControlError> {
        Ok(Self {
            phase: Phase::Greeting,
            incoming: MessageStream::new(),
            outgoing: vec![encode(&ClientMessage::Hello(hello))?],
            input: InputQueue::new(),
        })
    }

    /// How far the session has got.
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The next block of bytes to write to the control stream.
    pub fn poll_control(&mut self) -> Option<Vec<u8>> {
        if self.outgoing.is_empty() {
            None
        } else {
            Some(self.outgoing.remove(0))
        }
    }

    /// Feeds bytes read from the control stream and reports what changed.
    ///
    /// # Errors
    /// If the host broke the protocol. Every error here is fatal: the stream is
    /// ordered and reliable, so there is nothing to resynchronise to.
    pub fn control_bytes(&mut self, bytes: &[u8]) -> Result<Vec<Update>, SessionError> {
        self.incoming.push(bytes);
        let mut updates = Vec::new();
        while let Some(message) = self.incoming.next_message()? {
            updates.push(self.apply(message)?);
        }
        Ok(updates)
    }

    /// Records an input event, to be sent with the next batch.
    ///
    /// `at` is when the client saw the event, not when this was called, so that
    /// batching does not hide inside the latency measurement.
    pub fn input(&mut self, at: ClientTime, event: InputEvent) {
        self.input.push(at, event);
    }

    /// Ends an input tick, queueing whatever was recorded during it.
    ///
    /// Called on a short timer rather than per event, so that a burst of pointer
    /// motion costs one packet.
    ///
    /// # Errors
    /// If the batch does not fit a control message.
    pub fn flush_input(&mut self) -> Result<(), ControlError> {
        if let Some(batch) = self.input.take() {
            self.outgoing.push(encode(&ClientMessage::Input(batch))?);
        }
        Ok(())
    }

    /// Tells the host the video is now shown somewhere else, or at another size.
    ///
    /// # Errors
    /// If the message does not fit a control message.
    pub fn viewport_changed(&mut self, viewport: Viewport) -> Result<(), ControlError> {
        self.outgoing
            .push(encode(&ClientMessage::ViewportChanged(viewport))?);
        Ok(())
    }

    fn apply(&mut self, message: HostMessage) -> Result<Update, SessionError> {
        match message {
            HostMessage::Hello(host) => {
                if !matches!(self.phase, Phase::Greeting) {
                    return Err(SessionError::GreetedTwice);
                }
                if host.version != tachyx_proto::VERSION {
                    return Err(SessionError::VersionMismatch {
                        host: host.version,
                        ours: tachyx_proto::VERSION,
                    });
                }
                self.phase = Phase::Greeted(host.clone());
                Ok(Update::Greeted(host))
            }
            HostMessage::Stream(stream) => {
                let host = match &self.phase {
                    Phase::Greeting => return Err(SessionError::NoGreeting),
                    Phase::Greeted(host) | Phase::Streaming { host, .. } => host.clone(),
                };
                self.phase = Phase::Streaming { host, stream };
                Ok(Update::Stream(stream))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tachyx_proto::input::{PointerButton, Press};

    use super::*;
    use crate::control::{Codec, ResolutionMode, Size, TimedInput, encode};

    fn viewport() -> Viewport {
        Viewport {
            size: Size {
                width: 2400,
                height: 1080,
            },
            device_pixel_ratio: 2.75,
        }
    }

    fn hello() -> ClientHello {
        ClientHello::new(Vec::new(), viewport(), ResolutionMode::MatchViewport)
    }

    fn host_hello(version: u16) -> HostMessage {
        HostMessage::Hello(HostHello {
            version,
            monitors: Vec::new(),
        })
    }

    fn stream() -> StreamConfig {
        StreamConfig {
            codec: Codec::H264,
            coded: Size {
                width: 2400,
                height: 1088,
            },
            visible: Size {
                width: 2400,
                height: 1088,
            },
            pixel_aspect: (1, 1),
            frame_rate: 120,
            bitrate_bps: 25_000_000,
        }
    }

    fn sent(session: &mut Session) -> Vec<ClientMessage> {
        let mut bytes = Vec::new();
        while let Some(block) = session.poll_control() {
            bytes.extend(block);
        }
        let mut out = Vec::new();
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            out.push(postcard::from_bytes(&rest[4..4 + len]).expect("we wrote it"));
            rest = &rest[4 + len..];
        }
        out
    }

    #[test]
    fn the_hello_is_the_first_thing_written() {
        let mut session = Session::new(hello()).unwrap();
        assert_eq!(sent(&mut session), vec![ClientMessage::Hello(hello())]);
        assert_eq!(session.poll_control(), None);
    }

    #[test]
    fn a_greeting_then_a_configuration_reaches_streaming() {
        let mut session = Session::new(hello()).unwrap();
        let _ = sent(&mut session);

        let mut wire = encode(&host_hello(tachyx_proto::VERSION)).unwrap();
        wire.extend(encode(&HostMessage::Stream(stream())).unwrap());
        let updates = session.control_bytes(&wire).unwrap();

        assert_eq!(
            updates,
            vec![
                Update::Greeted(HostHello {
                    version: tachyx_proto::VERSION,
                    monitors: Vec::new()
                }),
                Update::Stream(stream()),
            ]
        );
        assert_eq!(session.phase().stream(), Some(&stream()));
    }

    #[test]
    fn a_reconfiguration_replaces_the_stream_and_keeps_the_host() {
        let mut session = Session::new(hello()).unwrap();
        session
            .control_bytes(&encode(&host_hello(tachyx_proto::VERSION)).unwrap())
            .unwrap();
        session
            .control_bytes(&encode(&HostMessage::Stream(stream())).unwrap())
            .unwrap();

        let smaller = StreamConfig {
            coded: Size {
                width: 1280,
                height: 720,
            },
            ..stream()
        };
        let updates = session
            .control_bytes(&encode(&HostMessage::Stream(smaller)).unwrap())
            .unwrap();
        assert_eq!(updates, vec![Update::Stream(smaller)]);
        assert_eq!(session.phase().stream(), Some(&smaller));
    }

    #[test]
    fn a_host_on_another_version_is_reported_not_misparsed() {
        let mut session = Session::new(hello()).unwrap();
        assert_eq!(
            session.control_bytes(&encode(&host_hello(tachyx_proto::VERSION + 1)).unwrap()),
            Err(SessionError::VersionMismatch {
                host: tachyx_proto::VERSION + 1,
                ours: tachyx_proto::VERSION,
            })
        );
    }

    #[test]
    fn a_host_that_streams_before_it_greets_is_refused() {
        let mut session = Session::new(hello()).unwrap();
        assert_eq!(
            session.control_bytes(&encode(&HostMessage::Stream(stream())).unwrap()),
            Err(SessionError::NoGreeting)
        );
    }

    #[test]
    fn input_leaves_only_when_the_tick_ends() {
        let mut session = Session::new(hello()).unwrap();
        let _ = sent(&mut session);

        session.input(
            ClientTime(1_000),
            InputEvent::PointerMotion { dx: 2.0, dy: 3.0 },
        );
        session.input(
            ClientTime(2_000),
            InputEvent::PointerMotion { dx: 4.0, dy: 1.0 },
        );
        session.input(
            ClientTime(3_000),
            InputEvent::PointerButton {
                button: PointerButton::Left,
                press: Press::Down,
            },
        );
        assert_eq!(session.poll_control(), None, "nothing leaves before a tick");

        session.flush_input().unwrap();
        assert_eq!(
            sent(&mut session),
            vec![ClientMessage::Input(vec![
                TimedInput {
                    at: ClientTime(2_000),
                    event: InputEvent::PointerMotion { dx: 6.0, dy: 4.0 },
                },
                TimedInput {
                    at: ClientTime(3_000),
                    event: InputEvent::PointerButton {
                        button: PointerButton::Left,
                        press: Press::Down,
                    },
                },
            ])],
            "one packet carries the whole burst, with the motion summed"
        );

        session.flush_input().unwrap();
        assert_eq!(session.poll_control(), None, "an empty tick sends nothing");
    }
}
