//! The session, as a state machine over messages rather than over bytes.
//!
//! [`Session`] performs no I/O and does no framing. It is handed the messages
//! that arrived, on the control stream or in a datagram, and asked for the
//! messages to send back, so the whole of the client's protocol behaviour is
//! exercised by a test with no network and no runtime. The connection
//! underneath, and the length prefix each control message travels behind,
//! belong to `tachyx-transport`.

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroU32};
use std::time::{Duration, Instant};

use tachyx_proto::control::{
    Capabilities, ClientHello, ClientMessage, Heartbeat, HostMessage, Size, StreamConfig,
    HEARTBEAT_INTERVAL,
};
use tachyx_proto::input::InputEvent;
use tachyx_proto::media::{FrameId, MalformedPacket, Timestamp};
use tachyx_proto::packet::{Frame, Reassembler, ReassemblyStats};

use crate::input::InputQueue;

/// How long a frame waits for the fragment that would complete it.
///
/// The fallback for the frame nothing follows: the host's capture blocks while
/// the screen is static, so the last frame before an idle period has no
/// successor to prove it will never be finished. Every other case is decided
/// sooner, by the reassembler's own reorder window.
const FRAME_DEADLINE: Duration = Duration::from_millis(50);

/// The shortest interval between two key frame requests.
///
/// The request itself travels on the reliable stream, so it never needs
/// repeating: a second one can only ask for a key frame the host is already
/// encoding. What this bounds is flapping, where a link losing packets steadily
/// breaks and repairs the reference chain over and over. A 1080p key frame was
/// measured at 130 KB, which is 130 ms of an 8 Mbit/s stream, so this is about
/// two key frames of wire time and a request inside it would be answered by a
/// frame already on its way.
const KEYFRAME_REQUEST_GAP: Duration = Duration::from_millis(250);

/// How long without a picture before the client says the screen is still rather
/// than that frames are arriving.
///
/// Twelve frame periods at sixty hertz. Long enough that a frame the transport
/// held back does not read as an idle screen, short enough to be well inside the
/// 1.53 second gap the host's capture was measured to leave.
const STILL: Duration = Duration::from_millis(200);

/// How long without a word from the host before the connection is called lost.
///
/// Three missed heartbeats. Two would fire on one lost wake-up on a busy phone,
/// and the transport's own five-second idle timeout is the next signal after
/// this one, so anything longer would make this report redundant.
const LOST_AFTER: Duration = HEARTBEAT_INTERVAL.saturating_mul(3);

/// The most the client will let the stream cost, in bits per second.
///
/// 1080p60 out of this host's encoder was measured at 8 Mbit/s, so this is
/// roughly twice what a full rate stream needs and leaves room for the burst a
/// key frame makes. It is a ceiling and not a target: the host's own rate
/// control spends less whenever the picture allows.
pub const BITRATE_CEILING: NonZeroU32 = NonZeroU32::new(20_000_000).unwrap();

/// What this client can drive, for a surface of `viewport` physical pixels on a
/// display that refreshes `refresh` times a second.
///
/// The decode limits are stated as the surface and the display rather than as
/// the decoder's own maxima. A picture larger than the surface is scaled down
/// before anybody sees it and a frame arriving faster than the display refreshes
/// is never shown, so either one spends bandwidth, decode time and battery on
/// something that cannot reach the user. The host's ladder is bounded by what is
/// said here, so saying more than this asks for waste.
#[must_use]
pub fn capabilities(viewport: Size, refresh: NonZeroU16) -> Capabilities {
    Capabilities {
        viewport,
        max_decode: viewport,
        max_decode_fps: refresh,
        requested_fps: refresh,
        bitrate_ceiling: BITRATE_CEILING,
    }
}

/// What the session is doing, as far as the client can tell.
///
/// Silence on the video path is not a symptom. The host's capture blocks while
/// the screen is static, with idle gaps measured up to 1.53 seconds, so a client
/// that reported "no frames" as a fault would report one every time the user
/// stopped to read something.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Liveness {
    /// Pictures are arriving.
    Streaming,
    /// The host is answering and the screen is not changing.
    #[default]
    Idle,
    /// The host has said nothing for three heartbeat intervals.
    Lost,
}

impl Liveness {
    /// What a session that last heard from the host at `heard`, and last had a
    /// picture at `saw`, is doing at `now`.
    ///
    /// A picture outranks a heartbeat: a host sending frames is alive whatever
    /// its heartbeat timer is doing.
    #[must_use]
    pub fn at(now: Instant, heard: Instant, saw: Option<Instant>) -> Self {
        if saw.is_some_and(|at| now.duration_since(at) < STILL) {
            Self::Streaming
        } else if now.duration_since(heard) < LOST_AFTER {
            Self::Idle
        } else {
            Self::Lost
        }
    }
}

/// A client session.
#[derive(Debug)]
pub struct Session {
    hello: ClientHello,
    stream: Option<StreamConfig>,
    outgoing: VecDeque<ClientMessage>,
    input: InputQueue,
    video: Reassembler,
    decoded: Option<FrameId>,
    asked_for_key: bool,
    ask_again_at: Instant,
    heard: Instant,
    saw: Option<Instant>,
    beat: Option<Heartbeat>,
}

impl Session {
    /// Opens a session that introduces itself with these capabilities.
    ///
    /// `now` starts the liveness clock, so a host that never answers is called
    /// lost three heartbeats after the connection opened rather than never.
    #[must_use]
    pub fn new(capabilities: Capabilities, now: Instant) -> Self {
        Self {
            hello: ClientHello::new(capabilities),
            stream: None,
            outgoing: VecDeque::new(),
            input: InputQueue::new(),
            video: Reassembler::new(FRAME_DEADLINE),
            decoded: None,
            // The reassembler starts wanting a key frame and the host starts by
            // sending one, so asking for it would be noise.
            asked_for_key: true,
            ask_again_at: now,
            heard: now,
            saw: None,
            beat: None,
        }
    }

    /// What the client says about itself, which the transport sends before
    /// anything else on the stream.
    #[must_use]
    pub const fn hello(&self) -> ClientHello {
        self.hello
    }

    /// How the host said it is encoding, once it has said.
    #[must_use]
    pub const fn stream(&self) -> Option<StreamConfig> {
        self.stream
    }

    /// The last thing the host said about itself.
    #[must_use]
    pub const fn heartbeat(&self) -> Option<Heartbeat> {
        self.beat
    }

    /// What the video path has seen.
    #[must_use]
    pub const fn video_stats(&self) -> ReassemblyStats {
        self.video.stats()
    }

    /// What the session is doing at `now`.
    #[must_use]
    pub fn liveness(&self, now: Instant) -> Liveness {
        Liveness::at(now, self.heard, self.saw)
    }

    /// The next message to send to the host.
    pub fn poll_control(&mut self) -> Option<ClientMessage> {
        self.outgoing.pop_front()
    }

    /// Takes one message the host sent.
    ///
    /// Any message is proof the host is alive, not only a heartbeat.
    pub fn host(&mut self, message: HostMessage, now: Instant) {
        self.heard = now;
        match message {
            HostMessage::Stream(stream) => self.stream = Some(stream),
            HostMessage::Heartbeat(beat) => self.beat = Some(beat),
        }
    }

    /// Takes one video datagram, returning the picture it completed.
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
            self.saw = Some(now);
        }
        self.request_key_if_needed(now);
        Ok(frame)
    }

    /// When the earliest incomplete frame gives up, if one is open.
    ///
    /// A receive loop waits on this rather than on a fixed tick, so a session
    /// watching a static screen costs no wake-ups.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.video.next_deadline()
    }

    /// Gives up on frames whose wait has run out.
    pub fn expire(&mut self, now: Instant) {
        self.video.expire(now);
        self.request_key_if_needed(now);
    }

    /// Records what the user did, to be sent with the next batch.
    ///
    /// `at` is when the client saw the event, not when this was called, so that
    /// batching does not hide inside the latency measurement.
    pub fn input(&mut self, at: Timestamp, event: InputEvent) {
        self.input.push(at, event);
    }

    /// Whether anything is waiting for the next tick to carry it.
    #[must_use]
    pub fn input_pending(&self) -> bool {
        !self.input.is_empty()
    }

    /// Ends an input tick, queueing whatever was recorded during it.
    ///
    /// Called on a short timer rather than per event, so that a drag at the
    /// touch sample rate costs one packet instead of one packet per sample.
    pub fn flush_input(&mut self) {
        if let Some(batch) = self.input.take() {
            self.outgoing.push_back(ClientMessage::Input(batch));
        }
    }

    /// Tells the host the client can take something different now, after a
    /// rotation, a resize, or a change of resolution mode.
    pub fn capabilities_changed(&mut self, capabilities: Capabilities) {
        self.outgoing
            .push_back(ClientMessage::ViewportChanged(capabilities));
    }

    /// Asks for a key frame on the edge where the decoder becomes stuck, and
    /// arms the next request on the edge where it is unstuck again.
    ///
    /// One request per lost packet would put a burst of them on the control
    /// stream and earn a burst of key frames back, and on this host every key
    /// frame is a full IDR: neither intra-refresh nor long-term reference frames
    /// are reachable through its encoder. [`KEYFRAME_REQUEST_GAP`] bounds the
    /// remaining case, a link breaking and repairing the chain repeatedly.
    fn request_key_if_needed(&mut self, now: Instant) {
        if !self.video.needs_keyframe() {
            self.asked_for_key = false;
            return;
        }
        // Already asked, or asked too recently. Neither forgets: the next
        // datagram or expiry asks again once the gap has passed.
        if self.asked_for_key || now < self.ask_again_at {
            return;
        }
        self.outgoing.push_back(ClientMessage::KeyframeRequest {
            after: self.decoded,
        });
        self.asked_for_key = true;
        self.ask_again_at = now + KEYFRAME_REQUEST_GAP;
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use tachyx_proto::control::{Aspect, Codec, CodedSize};
    use tachyx_proto::input::{PointerButton, Press};
    use tachyx_proto::media::FrameKind;
    use tachyx_proto::packet::{AccessUnit, Packetiser};

    use super::*;

    fn viewport() -> Size {
        Size::new(2400, 1080).expect("a size")
    }

    fn caps() -> Capabilities {
        capabilities(viewport(), NonZeroU16::new(120).expect("nonzero"))
    }

    fn session() -> Session {
        Session::new(caps(), Instant::now())
    }

    fn stream() -> StreamConfig {
        StreamConfig {
            codec: Codec::H264,
            coded: CodedSize::new(2400, 1088).expect("a codeable size"),
            visible: viewport(),
            display_aspect: Aspect::new(20, 9).expect("a ratio"),
            framerate: NonZeroU16::new(120).expect("nonzero"),
            bitrate: NonZeroU32::new(25_000_000).expect("nonzero"),
        }
    }

    fn queued(session: &mut Session) -> Vec<ClientMessage> {
        std::iter::from_fn(|| session.poll_control()).collect()
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

    fn payload() -> Vec<u8> {
        (0..5_000u32).map(|byte| byte.to_le_bytes()[0]).collect()
    }

    /// Loses a fragment, proves the loss with the next delta frame, and repairs
    /// the chain with a key frame. One whole break, and one edge. Reports how
    /// many key frame requests the break earned.
    fn break_and_repair(
        session: &mut Session,
        packetiser: &mut Packetiser,
        payload: &[u8],
        id: &mut u32,
        now: Instant,
    ) -> usize {
        let damaged = picture(packetiser, *id, FrameKind::Delta, payload);
        for datagram in &damaged[..damaged.len() - 1] {
            session.datagram(datagram, now).expect("a video packet");
        }
        *id += 1;
        for datagram in picture(packetiser, *id, FrameKind::Delta, payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }
        *id += 1;
        let asked = queued(session).len();
        for datagram in picture(packetiser, *id, FrameKind::Key, payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }
        *id += 1;
        let _ = queued(session);
        asked
    }

    #[test]
    fn the_client_asks_for_no_more_than_it_can_show() {
        let asked = caps();
        assert_eq!(asked.viewport, viewport());
        assert_eq!(
            asked.max_decode,
            viewport(),
            "a picture larger than the surface is scaled away before anyone sees it"
        );
        assert_eq!(asked.max_decode_fps.get(), 120);
        assert_eq!(asked.requested_fps.get(), 120);
        assert_eq!(asked.bitrate_ceiling, BITRATE_CEILING);
    }

    #[test]
    fn the_hello_carries_the_capabilities_and_nothing_is_queued_behind_it() {
        let mut session = session();
        assert_eq!(session.hello(), ClientHello::new(caps()));
        assert_eq!(
            session.poll_control(),
            None,
            "the transport sends the hello; the session does not queue it"
        );
    }

    #[test]
    fn a_stream_configuration_is_remembered_and_replaced() {
        let mut session = session();
        assert_eq!(session.stream(), None);

        let now = Instant::now();
        session.host(HostMessage::Stream(stream()), now);
        assert_eq!(session.stream(), Some(stream()));

        let smaller = StreamConfig {
            coded: CodedSize::new(1280, 720).expect("a codeable size"),
            visible: Size::new(1280, 720).expect("a size"),
            ..stream()
        };
        session.host(HostMessage::Stream(smaller), now);
        assert_eq!(
            session.stream(),
            Some(smaller),
            "a mid-session change replaces the geometry rather than adding to it"
        );
    }

    #[test]
    fn input_leaves_only_when_the_tick_ends() {
        let mut session = session();

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
        assert!(session.input_pending());
        assert_eq!(session.poll_control(), None, "nothing leaves before a tick");

        session.flush_input();
        assert!(!session.input_pending());
        let sent = queued(&mut session);
        assert_eq!(sent.len(), 1, "one packet carries the whole burst");
        let ClientMessage::Input(batch) = &sent[0] else {
            panic!("expected an input batch, got {:?}", sent[0]);
        };
        assert_eq!(batch.events.len(), 2, "the motion samples were summed");
        assert_eq!(
            batch.events[0].event,
            InputEvent::PointerMotion { dx: 6.0, dy: 4.0 }
        );

        session.flush_input();
        assert_eq!(session.poll_control(), None, "an empty tick sends nothing");
    }

    #[test]
    fn a_changed_viewport_is_renegotiated() {
        let mut session = session();
        let smaller = capabilities(
            Size::new(1080, 2400).expect("a size"),
            NonZeroU16::new(60).expect("nonzero"),
        );
        session.capabilities_changed(smaller);
        assert_eq!(
            queued(&mut session),
            vec![ClientMessage::ViewportChanged(smaller)]
        );
    }

    #[test]
    fn a_whole_picture_comes_back_out_of_its_datagrams() {
        let mut session = session();
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        let payload = payload();

        let mut delivered = None;
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            delivered = session
                .datagram(&datagram, Instant::now())
                .expect("a video packet");
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
        let mut session = session();
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        let payload = payload();
        let now = Instant::now();

        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }
        let _ = queued(&mut session);

        // Lose the last fragment of the next picture, then deliver the one after
        // it: a later frame arriving is what proves the earlier one will never
        // be finished.
        let damaged = picture(&mut packetiser, 1, FrameKind::Delta, &payload);
        for datagram in &damaged[..damaged.len() - 1] {
            session.datagram(datagram, now).expect("a video packet");
        }
        for datagram in picture(&mut packetiser, 2, FrameKind::Delta, &payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }

        assert_eq!(
            queued(&mut session),
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
            recovered = session.datagram(&datagram, now).expect("a video packet");
        }
        assert!(recovered.is_some(), "the key frame ends the outage");
        assert_eq!(session.poll_control(), None, "recovery is not a request");
    }

    #[test]
    fn a_link_that_breaks_repeatedly_cannot_spam_the_host() {
        let mut session = session();
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        let payload = payload();
        let start = Instant::now();
        let mut id = 0;

        // The first break happens before anything has decoded, so it is the one
        // the session was built already expecting a key frame for.
        for datagram in picture(&mut packetiser, id, FrameKind::Key, &payload) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        id += 1;

        // Ten breaks and ten repairs, every one a fresh edge, packed into one
        // request gap.
        let requests: usize = (0..10u32)
            .map(|round| {
                let now = start + Duration::from_millis(u64::from(round) * 25);
                break_and_repair(&mut session, &mut packetiser, &payload, &mut id, now)
            })
            .sum();
        assert_eq!(
            requests, 1,
            "ten breaks inside one {KEYFRAME_REQUEST_GAP:?} gap earn one request"
        );

        // Past the gap, the next break is asked about again.
        let now = start + KEYFRAME_REQUEST_GAP + Duration::from_millis(1);
        assert_eq!(
            break_and_repair(&mut session, &mut packetiser, &payload, &mut id, now),
            1,
            "the gap has passed"
        );
    }

    #[test]
    fn a_still_screen_is_idle_and_not_a_lost_host() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        assert_eq!(session.liveness(start), Liveness::Idle);

        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload()) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        assert_eq!(session.liveness(start), Liveness::Streaming);

        // The measured worst case: the host's capture blocks for 1.53 seconds on
        // a screen nothing is drawing to, and says so on the control stream.
        let beat = Heartbeat {
            sent: Timestamp::from_session_micros(1_000_000),
            last_frame: Some(FrameId(0)),
            frames_sent: 1,
            frames_gated: 0,
            pacing_rate: None,
            pipeline_micros: 4_200,
        };
        session.host(HostMessage::Heartbeat(beat), start + HEARTBEAT_INTERVAL);
        session.host(HostMessage::Heartbeat(beat), start + HEARTBEAT_INTERVAL * 2);
        session.host(HostMessage::Heartbeat(beat), start + HEARTBEAT_INTERVAL * 3);
        assert_eq!(
            session.liveness(start + Duration::from_millis(1_530)),
            Liveness::Idle,
            "1.53 seconds without a frame is a still screen, not a dead host"
        );
        assert_eq!(session.heartbeat(), Some(beat));
    }

    #[test]
    fn a_host_that_stops_speaking_is_reported_lost() {
        let start = Instant::now();
        let session = Session::new(caps(), start);
        assert_eq!(session.liveness(start + LOST_AFTER), Liveness::Lost);
        assert_eq!(
            session.liveness(start + HEARTBEAT_INTERVAL * 2),
            Liveness::Idle,
            "two missed heartbeats are not yet a fault"
        );
    }
}
