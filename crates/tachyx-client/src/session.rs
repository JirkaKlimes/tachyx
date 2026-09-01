//! The session, as a state machine that moves bytes rather than sockets.
//!
//! [`Session`] performs no I/O. It is handed the bytes that arrived, on the
//! control stream or in a datagram, and asked for the bytes to write back, so
//! the whole of the client's protocol behaviour is exercised by a test with no
//! network and no runtime. The QUIC connection underneath belongs to
//! `tachyx-transport`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tachyx_proto::control::{
    Capabilities, ClientHello, ClientMessage, Heartbeat, HostMessage, StreamConfig,
};
use tachyx_proto::input::InputEvent;
use tachyx_proto::media::{FrameId, MalformedPacket, Timestamp};
use tachyx_proto::packet::{Frame, Reassembler, ReassemblyStats};

use crate::framing::{self, FramingError, MessageStream};
use crate::input::InputQueue;

/// How long a frame waits for the fragment that would complete it.
///
/// The fallback for the frame nothing follows: the host's capture blocks while
/// the screen is static, so the last frame before an idle period has no
/// successor to prove it will never be finished. Every other case is decided
/// sooner, by the reassembler's own reorder window.
const FRAME_DEADLINE: Duration = Duration::from_millis(50);

/// How far the session has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The hello has been queued and the host has not said how it will encode.
    Greeting,
    /// The host is encoding like this.
    Streaming(StreamConfig),
}

impl Phase {
    /// What the host is currently sending, if it has said.
    #[must_use]
    pub const fn stream(self) -> Option<StreamConfig> {
        match self {
            Self::Streaming(stream) => Some(stream),
            Self::Greeting => None,
        }
    }
}

/// Something the rest of the client has to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The host will encode like this from now on. Arrives again on every
    /// change, including one forced by congestion, and reconfigures the decoder.
    Stream(StreamConfig),
    /// The host is alive, and this is what it has been doing. Sent on a fixed
    /// interval, because silence on the video path means an idle host as often
    /// as a dead one.
    Alive(Heartbeat),
}

/// A client session.
#[derive(Debug)]
pub struct Session {
    phase: Phase,
    incoming: MessageStream,
    outgoing: VecDeque<Vec<u8>>,
    input: InputQueue,
    video: Reassembler,
    decoded: Option<FrameId>,
    asked_for_key: bool,
}

impl Session {
    /// Opens a session that introduces itself with these capabilities.
    ///
    /// The hello is queued immediately, so the first [`Session::poll_control`]
    /// hands back the bytes to write on the freshly opened stream.
    ///
    /// # Errors
    /// If the hello does not fit a control message, which nothing a real client
    /// can describe reaches.
    pub fn new(capabilities: Capabilities) -> Result<Self, FramingError> {
        let hello = framing::frame(&ClientHello::new(capabilities))?;
        Ok(Self {
            phase: Phase::Greeting,
            incoming: MessageStream::new(),
            outgoing: VecDeque::from([hello]),
            input: InputQueue::new(),
            video: Reassembler::new(FRAME_DEADLINE),
            decoded: None,
            // The reassembler starts wanting a key frame and the host starts by
            // sending one, so asking for it would be noise.
            asked_for_key: true,
        })
    }

    /// How far the session has got.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// What the video path has seen.
    #[must_use]
    pub const fn video_stats(&self) -> ReassemblyStats {
        self.video.stats()
    }

    /// The next block of bytes to write to the control stream.
    pub fn poll_control(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }

    /// Feeds bytes read from the control stream and reports what changed.
    ///
    /// # Errors
    /// If the host broke the framing. Fatal: the stream is ordered and reliable,
    /// so there is nothing left to resynchronise to.
    pub fn control_bytes(&mut self, bytes: &[u8]) -> Result<Vec<Update>, FramingError> {
        self.incoming.push(bytes);
        let mut updates = Vec::new();
        while let Some(message) = self.incoming.next_message()? {
            updates.push(match message {
                HostMessage::Stream(stream) => {
                    self.phase = Phase::Streaming(stream);
                    Update::Stream(stream)
                }
                HostMessage::Heartbeat(beat) => Update::Alive(beat),
            });
        }
        Ok(updates)
    }

    /// Takes one video datagram, returning the picture it completed.
    ///
    /// Loss that leaves the decoder stuck queues one key frame request, not one
    /// per lost packet.
    ///
    /// # Errors
    /// If the datagram is not a video packet.
    pub fn datagram(
        &mut self,
        datagram: &[u8],
        now: Instant,
    ) -> Result<Option<Frame>, MalformedPacket> {
        let frame = self.video.push(datagram, now)?;
        if let Some(frame) = &frame {
            self.decoded = Some(frame.id);
        }
        self.request_key_if_needed();
        Ok(frame)
    }

    /// Gives up on frames whose wait has run out.
    ///
    /// Only needed on a connection where no datagram is arriving to run it, so
    /// on a timer beside the receive loop rather than in it.
    pub fn expire(&mut self, now: Instant) {
        self.video.expire(now);
        self.request_key_if_needed();
    }

    /// Records what the user did, to be sent with the next batch.
    ///
    /// `at` is when the client saw the event, not when this was called, so that
    /// batching does not hide inside the latency measurement.
    pub fn input(&mut self, at: Timestamp, event: InputEvent) {
        self.input.push(at, event);
    }

    /// Ends an input tick, queueing whatever was recorded during it.
    ///
    /// Called on a short timer rather than per event, so that a drag at the
    /// touch sample rate costs one packet instead of one packet per sample.
    ///
    /// # Errors
    /// If the batch does not fit a control message.
    pub fn flush_input(&mut self) -> Result<(), FramingError> {
        if let Some(batch) = self.input.take() {
            self.send(&ClientMessage::Input(batch))?;
        }
        Ok(())
    }

    /// Tells the host the client can take something different now, after a
    /// rotation, a resize, or a change of resolution mode.
    ///
    /// # Errors
    /// If the message does not fit a control message.
    pub fn capabilities_changed(&mut self, capabilities: Capabilities) -> Result<(), FramingError> {
        self.send(&ClientMessage::ViewportChanged(capabilities))
    }

    fn send(&mut self, message: &ClientMessage) -> Result<(), FramingError> {
        self.outgoing.push_back(framing::frame(message)?);
        Ok(())
    }

    /// Asks for a key frame on the edge where the decoder becomes stuck, and
    /// arms the next request on the edge where it is unstuck again.
    ///
    /// One request per lost packet would put a burst of them on the control
    /// stream and earn a burst of key frames back, and on this host every key
    /// frame is a full IDR: neither intra-refresh nor long-term reference frames
    /// are reachable through its encoder.
    fn request_key_if_needed(&mut self) {
        match (self.video.needs_keyframe(), self.asked_for_key) {
            (true, false) => {
                let request = ClientMessage::KeyframeRequest {
                    after: self.decoded,
                };
                if let Ok(bytes) = framing::frame(&request) {
                    self.outgoing.push_back(bytes);
                    self.asked_for_key = true;
                }
            }
            (false, true) => self.asked_for_key = false,
            (true, true) | (false, false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};

    use tachyx_proto::control::{Aspect, Codec, CodedSize, Size};
    use tachyx_proto::input::{PointerButton, Press};
    use tachyx_proto::media::FrameKind;
    use tachyx_proto::packet::{AccessUnit, Packetiser};

    use super::*;

    fn capabilities() -> Capabilities {
        Capabilities {
            viewport: Size::new(2400, 1080).unwrap(),
            max_decode: Size::new(3840, 2160).unwrap(),
            max_decode_fps: NonZeroU16::new(120).unwrap(),
            requested_fps: NonZeroU16::new(120).unwrap(),
            bitrate_ceiling: NonZeroU32::new(25_000_000).unwrap(),
        }
    }

    fn stream() -> StreamConfig {
        StreamConfig {
            codec: Codec::H264,
            coded: CodedSize::new(2400, 1088).unwrap(),
            visible: Size::new(2400, 1080).unwrap(),
            display_aspect: Aspect::new(20, 9).unwrap(),
            framerate: NonZeroU16::new(120).unwrap(),
            bitrate: NonZeroU32::new(25_000_000).unwrap(),
        }
    }

    /// The bodies of everything the session has queued, with the length prefix
    /// taken off.
    fn queued(session: &mut Session) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(block) = session.poll_control() {
            out.push(block[4..].to_vec());
        }
        out
    }

    fn messages(session: &mut Session) -> Vec<ClientMessage> {
        queued(session)
            .iter()
            .map(|body| postcard::from_bytes(body).expect("this client wrote it"))
            .collect()
    }

    /// The datagrams of one picture, as the host would put them on the wire.
    fn picture(
        packetiser: &mut Packetiser,
        id: u32,
        kind: FrameKind,
        payload: &[u8],
    ) -> Vec<Vec<u8>> {
        let slices = [payload];
        let unit = AccessUnit::new(
            FrameId(id),
            Timestamp::from_session_micros(u64::from(id) * 8_333),
            kind,
            &slices,
        )
        .expect("one non-empty slice is a picture");
        let mut out = Vec::new();
        packetiser.packetise(&unit, &mut out);
        out
    }

    #[test]
    fn the_hello_is_the_first_thing_written() {
        let mut session = Session::new(capabilities()).unwrap();
        let first = queued(&mut session);
        assert_eq!(first.len(), 1);
        assert_eq!(
            ClientHello::decode(&first[0]),
            Ok(ClientHello::new(capabilities()))
        );
        assert_eq!(session.poll_control(), None);
    }

    #[test]
    fn a_stream_configuration_moves_the_session_to_streaming() {
        let mut session = Session::new(capabilities()).unwrap();
        let _ = queued(&mut session);
        let updates = session
            .control_bytes(&framing::frame(&HostMessage::Stream(stream())).unwrap())
            .unwrap();
        assert_eq!(updates, vec![Update::Stream(stream())]);
        assert_eq!(session.phase(), Phase::Streaming(stream()));
    }

    #[test]
    fn a_reconfiguration_replaces_the_stream() {
        let mut session = Session::new(capabilities()).unwrap();
        session
            .control_bytes(&framing::frame(&HostMessage::Stream(stream())).unwrap())
            .unwrap();
        let smaller = StreamConfig {
            coded: CodedSize::new(1280, 720).unwrap(),
            visible: Size::new(1280, 720).unwrap(),
            ..stream()
        };
        let updates = session
            .control_bytes(&framing::frame(&HostMessage::Stream(smaller)).unwrap())
            .unwrap();
        assert_eq!(updates, vec![Update::Stream(smaller)]);
        assert_eq!(session.phase().stream(), Some(smaller));
    }

    #[test]
    fn input_leaves_only_when_the_tick_ends() {
        let mut session = Session::new(capabilities()).unwrap();
        let _ = queued(&mut session);

        session.input(
            Timestamp::from_session_micros(1_000),
            InputEvent::PointerMotion { dx: 2.0, dy: 3.0 },
        );
        session.input(
            Timestamp::from_session_micros(2_000),
            InputEvent::PointerMotion { dx: 4.0, dy: 1.0 },
        );
        session.input(
            Timestamp::from_session_micros(3_000),
            InputEvent::PointerButton {
                button: PointerButton::Left,
                press: Press::Down,
            },
        );
        assert_eq!(session.poll_control(), None, "nothing leaves before a tick");

        session.flush_input().unwrap();
        let sent = messages(&mut session);
        assert_eq!(sent.len(), 1, "one packet carries the whole burst");
        let ClientMessage::Input(batch) = &sent[0] else {
            panic!("expected an input batch, got {:?}", sent[0]);
        };
        assert_eq!(batch.events.len(), 2, "the motion samples were summed");
        assert_eq!(
            batch.events[0].event,
            InputEvent::PointerMotion { dx: 6.0, dy: 4.0 }
        );

        session.flush_input().unwrap();
        assert_eq!(session.poll_control(), None, "an empty tick sends nothing");
    }

    #[test]
    fn a_whole_picture_comes_back_out_of_its_datagrams() {
        let mut session = Session::new(capabilities()).unwrap();
        let _ = queued(&mut session);
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).unwrap());
        let payload: Vec<u8> = (0..5_000u32).map(|byte| byte.to_le_bytes()[0]).collect();

        let mut delivered = None;
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            delivered = session.datagram(&datagram, Instant::now()).unwrap();
        }
        let frame = delivered.expect("the last datagram completes the picture");
        assert_eq!(frame.id, FrameId(0));
        assert_eq!(frame.kind, FrameKind::Key);
        assert_eq!(frame.data, payload);
        assert_eq!(session.video_stats().frames_delivered, 1);
        assert_eq!(
            session.poll_control(),
            None,
            "nothing was lost, so nothing is asked for"
        );
    }

    #[test]
    fn loss_asks_for_one_key_frame_and_not_one_per_packet() {
        let mut session = Session::new(capabilities()).unwrap();
        let _ = queued(&mut session);
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).unwrap());
        let payload: Vec<u8> = (0..5_000u32).map(|byte| byte.to_le_bytes()[0]).collect();
        let now = Instant::now();

        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, now).unwrap();
        }
        let _ = queued(&mut session);

        // Lose the last fragment of the next picture, then deliver the one after
        // it: a later frame arriving is what proves the earlier one will never
        // be finished.
        let damaged = picture(&mut packetiser, 1, FrameKind::Delta, &payload);
        for datagram in &damaged[..damaged.len() - 1] {
            session.datagram(datagram, now).unwrap();
        }
        for datagram in picture(&mut packetiser, 2, FrameKind::Delta, &payload) {
            session.datagram(&datagram, now).unwrap();
        }

        assert_eq!(
            messages(&mut session),
            vec![ClientMessage::KeyframeRequest {
                after: Some(FrameId(0))
            }],
            "one request, naming the last picture that did decode"
        );

        // More loss while the decoder is already stuck asks for nothing more.
        session.expire(now + Duration::from_secs(2));
        assert_eq!(session.poll_control(), None);

        // The key frame that answers it arms the next request without sending
        // one.
        let mut recovered = None;
        for datagram in picture(&mut packetiser, 3, FrameKind::Key, &payload) {
            recovered = session.datagram(&datagram, now).unwrap();
        }
        assert!(recovered.is_some(), "the key frame ends the outage");
        assert_eq!(session.poll_control(), None, "recovery is not a request");
    }
}
