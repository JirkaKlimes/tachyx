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
    Capabilities, ClientHello, ClientMessage, FEEDBACK_INTERVAL, FEEDBACK_SLOW_BELOW,
    FEEDBACK_SLOW_INTERVAL, HEARTBEAT_INTERVAL, Heartbeat, HostMessage, Size, StreamConfig,
};
use tachyx_proto::input::InputEvent;
use tachyx_proto::media::{FrameId, MalformedPacket, Timestamp};
use tachyx_proto::packet::{Frame, Reassembler, ReassemblyStats};
use tachyx_proto::report::Reporter;

use crate::cursor::{CursorTracker, CursorView, Spot};
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
/// The request arrives: it travels on the reliable stream. Being answered is
/// another matter. The host draws every cause of a key frame from one budget, so
/// a bitrate step taken a moment earlier makes it refuse this one and say
/// nothing, and it holds no record of the refusal to act on later. A client that
/// asked once would then wait for a picture nobody is going to send, for the
/// rest of the session. So the request is repeated for as long as the decoder is
/// stuck, and this is what bounds the repetition.
///
/// A 1080p key frame was measured at 130 KB, which is 130 ms of an 8 Mbit/s
/// stream, so this is about two key frames of wire time: a request inside it
/// would be answered by a frame already on its way. It also bounds flapping,
/// where a link losing packets steadily breaks and repairs the reference chain
/// over and over.
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
/// Generous, because a heartbeat is a stream frame and video datagrams are
/// written into a packet ahead of stream frames. A saturated link therefore
/// delays heartbeats long before it drops the connection, and a tight deadline
/// here reports a dead host when the truth is a busy one. Measured on Wi-Fi:
/// three missed beats fired within seconds while video was still arriving.
///
/// The transport's own idle timeout is the next signal after this one, so this
/// stays below it and above anything congestion alone can cause.
const LOST_AFTER: Duration = HEARTBEAT_INTERVAL.saturating_mul(16);

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
    pictures: VecDeque<Frame>,
    decoded: Option<FrameId>,
    ask_again_at: Instant,
    heard: Instant,
    saw: Option<Instant>,
    /// Frames the reassembler had decided, decodable or not, when `saw` was
    /// last moved. See [`Session::collect`].
    arrived: u64,
    beat: Option<Heartbeat>,
    reports: Reporter,
    cursor: CursorTracker,
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
            pictures: VecDeque::new(),
            decoded: None,
            // The reassembler starts wanting a key frame and the host starts by
            // sending one, so asking inside the first gap would be noise. Asking
            // after it is not: that key frame is the largest frame of the
            // session and therefore the likeliest to be lost, and a client that
            // stayed quiet about it would show nothing for as long as it stayed
            // connected.
            ask_again_at: now + KEYFRAME_REQUEST_GAP,
            heard: now,
            saw: None,
            arrived: 0,
            beat: None,
            reports: Reporter::new(now),
            cursor: CursorTracker::new(),
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

    /// When the host was last heard from.
    ///
    /// Published for a reader outside the session that has to work out
    /// [`Liveness`] for itself. The answer moves with the clock rather than with
    /// any event, so a liveness value recorded when something last happened
    /// would be stale in exactly the case that matters: nothing happening.
    #[must_use]
    pub const fn heard_at(&self) -> Instant {
        self.heard
    }

    /// When a picture last arrived, if one has. See [`Session::heard_at`].
    #[must_use]
    pub const fn saw_at(&self) -> Option<Instant> {
        self.saw
    }

    /// The next message to send to the host.
    pub fn poll_control(&mut self) -> Option<ClientMessage> {
        self.outgoing.pop_front()
    }

    /// Takes one message the host sent.
    ///
    /// Any message is proof the host is alive, not only a heartbeat.
    pub fn host(&mut self, message: HostMessage, now: Instant, round_trip: Duration) {
        self.heard = now;
        match message {
            HostMessage::Stream(stream) => {
                self.stream = Some(stream);
                // A damaged frame waits one frame period for a reordered
                // packet or its repair. The shipped floor is one period at
                // 60 Hz; at the 20 frames a second a narrow link is ladders
                // down to, holding the floor instead was measured throwing
                // away frames whose parity was still in flight, 429 datagrams
                // counted late against 211 counted lost.
                self.video
                    .set_reorder_window(Duration::from_secs(1) / u32::from(stream.framerate.get()));
            }
            HostMessage::Heartbeat(beat) => self.beat = Some(beat),
            HostMessage::Cursor(update) => self.cursor.reported(now, &update, round_trip),
        }
    }

    /// Where to draw the host's pointer, or `None` when there is nothing to
    /// draw.
    ///
    /// The position is the client's own, moved by the user's own input and
    /// corrected by what the host reports, so reading it costs no round trip.
    #[must_use]
    pub fn cursor(&self) -> Option<CursorView> {
        self.cursor.view()
    }

    /// Takes one video datagram.
    ///
    /// Whatever it completed, or the parity behind it rebuilt, comes out of
    /// [`Session::pop_frame`]. One datagram can finish more than one picture,
    /// so a caller drains rather than reading a single return.
    ///
    /// # Errors
    /// If the datagram is not a video packet.
    pub fn datagram(&mut self, datagram: &[u8], now: Instant) -> Result<(), MalformedPacket> {
        let seq = self.video.push(datagram, now)?;
        self.reports.record(seq, now);
        self.collect(now);
        self.request_key_if_needed(now);
        self.report(now);
        Ok(())
    }

    /// The next picture ready to decode, in frame order.
    ///
    /// Drained after every [`Session::datagram`] and every [`Session::expire`].
    /// A picture the caller never takes is still a picture that arrived, so
    /// liveness and the identifier a key frame request names are settled when
    /// the picture is finished rather than when it is taken.
    pub fn pop_frame(&mut self) -> Option<Frame> {
        self.pictures.pop_front()
    }

    /// Takes everything reassembly has finished, and records what arrived.
    ///
    /// What moves `saw`, and with it [`Liveness::Streaming`], is every frame
    /// the reassembler decided with data in hand: delivered, thrown away
    /// undecodable, or dropped short of repair. Only the decodable ones reach
    /// the decoder, but liveness answers "is the screen changing", and a frame
    /// that arrived and could not be decoded is a changing screen. Counting
    /// decodable frames alone is how a session waiting out a broken reference
    /// chain reported an idle screen while hundreds of frames a minute were
    /// arriving: 413 of 848 in one measured run.
    fn collect(&mut self, now: Instant) {
        let stats = self.video.stats();
        let arrived = stats.frames_delivered + stats.frames_undecodable + stats.frames_dropped;
        if arrived > self.arrived {
            self.arrived = arrived;
            self.saw = Some(now);
        }
        while let Some(frame) = self.video.pop() {
            self.decoded = Some(frame.id);
            self.pictures.push_back(frame);
        }
    }

    /// When the session next has something to do without being spoken to.
    ///
    /// Two things: the earliest incomplete frame giving up, and the next key
    /// frame request while the decoder is stuck. The second matters because the
    /// host can stop sending video altogether, and a client that only revisited
    /// the question on the arrival of a datagram would then never revisit it.
    ///
    /// A receive loop waits on this rather than on a fixed tick, so a session
    /// watching a static screen costs no wake-ups.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.video.next_deadline(),
            self.video.needs_keyframe().then_some(self.ask_again_at),
            self.reports.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Gives up on frames whose wait has run out.
    pub fn expire(&mut self, now: Instant) {
        self.video.expire(now);
        self.collect(now);
        self.request_key_if_needed(now);
        self.report(now);
    }

    /// Queues whatever the host is owed about the path.
    ///
    /// Drained in a loop because a report carries at most 512 statuses and a
    /// longer one is split into parts sent back to back. The counters in it
    /// are the reporter's own, which forgive a packet that was merely
    /// reordered; the reassembler's counters do not, and feeding them here was
    /// how a link that reorders a fifth of its packets was throttled for loss
    /// it was not suffering.
    fn report(&mut self, now: Instant) {
        // The formula libwebrtc uses returns something under 50 ms on any link
        // above 218 kbit/s, so the clamp decides everywhere except on a
        // collapsing link, and this is the one comparison that reproduces it.
        let interval = match self.stream {
            Some(stream) if stream.bitrate.get() < FEEDBACK_SLOW_BELOW => FEEDBACK_SLOW_INTERVAL,
            _ => FEEDBACK_INTERVAL,
        };
        while let Some(report) = self.reports.poll(now, interval) {
            self.outgoing.push_back(ClientMessage::Feedback(report));
        }
    }

    /// Records what the user did, to be sent with the next batch.
    ///
    /// `at` is when the client saw the event, not when this was called, so that
    /// batching does not hide inside the latency measurement.
    pub fn input(&mut self, at: Timestamp, now: Instant, event: InputEvent) {
        self.predict(now, &event);
        self.input.push(at, event);
    }

    /// Moves the client's own pointer by what the user just did.
    ///
    /// The wire carries a displacement in pixels of the streamed picture, so
    /// dividing by that picture is what turns it into the units the tracker
    /// works in. What the host then does with it is measured rather than
    /// assumed: see [`crate::cursor::CursorTracker`].
    fn predict(&mut self, now: Instant, event: &InputEvent) {
        let Some(stream) = self.stream else {
            // Nothing is drawn before the host has said how it is encoding, so
            // there is nothing to move yet.
            return;
        };
        let across = f32::from(stream.visible.width.get());
        let down = f32::from(stream.visible.height.get());
        match *event {
            InputEvent::PointerMotion { dx, dy } => {
                self.cursor.moved(now, dx / across, dy / down);
            }
            InputEvent::PointerMotionAbsolute { x, y } => {
                self.cursor.placed(now, Spot { x, y });
            }
            _ => {}
        }
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

    /// Says a decoder has appeared after pictures were already thrown away.
    ///
    /// Those pictures reached the client whole, so reassembly saw nothing wrong
    /// and has no reason to ask for anything; only the caller knows they went
    /// nowhere. Without this a session whose decoder arrived one picture late
    /// shows nothing until the next key frame, and on a stream that is losing
    /// nothing the next key frame can be minutes away.
    pub fn decoder_started_late(&mut self, now: Instant) {
        self.outgoing
            .push_back(ClientMessage::KeyframeRequest { after: None });
        self.ask_again_at = now + KEYFRAME_REQUEST_GAP;
    }

    /// Asks for a key frame while the decoder is stuck, no more often than
    /// [`KEYFRAME_REQUEST_GAP`].
    ///
    /// Level-triggered, not edge-triggered. The host is free to refuse a request
    /// and keeps no record of having refused one, so a client that asked on the
    /// edge where it became stuck and then went quiet would stay stuck for the
    /// rest of the session on a single refusal.
    ///
    /// One request per lost packet would go the other way and put a burst of
    /// them on the control stream, earning a burst of key frames back, and on
    /// this host every key frame is a full IDR: neither intra-refresh nor
    /// long-term reference frames are reachable through its encoder. The gap is
    /// what holds both ends.
    fn request_key_if_needed(&mut self, now: Instant) {
        if !self.video.needs_keyframe() || now < self.ask_again_at {
            return;
        }
        self.outgoing.push_back(ClientMessage::KeyframeRequest {
            after: self.decoded,
        });
        self.ask_again_at = now + KEYFRAME_REQUEST_GAP;
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use tachyx_proto::control::{
        Aspect, Codec, CodedSize, FEEDBACK_INTERVAL, FEEDBACK_SLOW_BELOW, FEEDBACK_SLOW_INTERVAL,
        Feedback, PacketArrival,
    };
    use tachyx_proto::input::{PointerButton, Press};
    use tachyx_proto::media::FrameKind;
    use tachyx_proto::media::Seq;
    use tachyx_proto::packet::{AccessUnit, Packetiser, Parity};

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

    /// A round trip for the tests that hand the session a host message. Only
    /// the cursor reads it, and only to age a report forward.
    const RTT: Duration = Duration::from_millis(10);

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

    /// A host that sends no parity, because these tests induce loss by
    /// removing the last datagram of a picture and parity would repair it. The
    /// repair itself is tested where it lives, in `tachyx_proto::packet`.
    fn bare_packetiser() -> Packetiser {
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        packetiser.set_parity(Parity::fixed(0));
        packetiser
    }

    /// Everything the session wants to send, less the receiver reports.
    ///
    /// A datagram that arrives is a datagram whose arrival has to be reported,
    /// so every test below that feeds a picture would otherwise have to step
    /// around one. What the reports themselves say is asserted in its own test.
    fn queued(session: &mut Session) -> Vec<ClientMessage> {
        std::iter::from_fn(|| session.poll_control())
            .filter(|message| !matches!(message, ClientMessage::Feedback(_)))
            .collect()
    }

    fn reports(session: &mut Session) -> Vec<Feedback> {
        std::iter::from_fn(|| session.poll_control())
            .filter_map(|message| match message {
                ClientMessage::Feedback(report) => Some(report),
                _ => None,
            })
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
        session.host(HostMessage::Stream(stream()), now, RTT);
        assert_eq!(session.stream(), Some(stream()));

        let smaller = StreamConfig {
            coded: CodedSize::new(1280, 720).expect("a codeable size"),
            visible: Size::new(1280, 720).expect("a size"),
            ..stream()
        };
        session.host(HostMessage::Stream(smaller), now, RTT);
        assert_eq!(
            session.stream(),
            Some(smaller),
            "a mid-session change replaces the geometry rather than adding to it"
        );
    }

    #[test]
    fn input_leaves_only_when_the_tick_ends() {
        let mut session = session();

        let now = Instant::now();
        session.input(
            Timestamp::from_session_micros(1_000),
            now,
            InputEvent::PointerMotion { dx: 2.0, dy: 3.0 },
        );
        session.input(
            Timestamp::from_session_micros(2_000),
            now,
            InputEvent::PointerMotion { dx: 4.0, dy: 1.0 },
        );
        session.input(
            Timestamp::from_session_micros(3_000),
            now,
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
        let mut packetiser = bare_packetiser();
        let payload = payload();

        let now = Instant::now();
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }
        let frame = session
            .pop_frame()
            .expect("the last datagram completes the picture");
        assert_eq!(frame.id, FrameId(0));
        assert_eq!(frame.kind, FrameKind::Key);
        assert_eq!(frame.data, payload);
        assert_eq!(session.video_stats().frames_delivered, 1);
        assert!(
            queued(&mut session).is_empty(),
            "nothing was lost, so nothing is asked for"
        );
    }

    #[test]
    fn loss_asks_for_one_key_frame_and_not_one_per_packet() {
        let opened = Instant::now();
        let mut session = Session::new(caps(), opened);
        let mut packetiser = bare_packetiser();
        let payload = payload();
        // The opening key frame arrives inside the gap the session opens with,
        // where the one the host sends unasked is waited for rather than asked
        // about. Everything after it is past that gap.
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, opened).expect("a video packet");
        }
        assert!(queued(&mut session).is_empty(), "nothing to ask about yet");
        let now = opened + KEYFRAME_REQUEST_GAP;

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

        // More loss inside the gap asks for nothing more.
        session.expire(now + Duration::from_millis(1));
        assert!(queued(&mut session).is_empty());

        // Past it, the request is repeated: the host may have refused the first
        // one, and it holds no record of having done so.
        session.expire(now + KEYFRAME_REQUEST_GAP);
        assert_eq!(
            queued(&mut session),
            vec![ClientMessage::KeyframeRequest {
                after: Some(FrameId(0))
            }]
        );

        // The key frame that answers it arms the next request without sending
        // one.
        for datagram in picture(&mut packetiser, 3, FrameKind::Key, &payload) {
            session.datagram(&datagram, now).expect("a video packet");
        }
        assert!(
            session.pop_frame().is_some(),
            "the key frame ends the outage"
        );
        assert!(queued(&mut session).is_empty(), "recovery is not a request");
    }

    #[test]
    fn a_key_frame_the_host_refuses_is_asked_for_again() {
        // The host may refuse: its key frame budget is shared with the bitrate
        // and resolution causes, and a bitrate step spends it. A client that
        // asks once and then waits shows a broken picture for the rest of the
        // session, because nothing else on either side ever revisits the
        // decision.
        let opened = Instant::now();
        let mut session = Session::new(caps(), opened);
        let mut packetiser = bare_packetiser();
        let payload = payload();
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, opened).expect("a video packet");
        }
        let start = opened + KEYFRAME_REQUEST_GAP;

        // Break the chain and take the one request it earns.
        let damaged = picture(&mut packetiser, 1, FrameKind::Delta, &payload);
        for datagram in &damaged[..damaged.len() - 1] {
            session.datagram(datagram, start).expect("a video packet");
        }
        for datagram in picture(&mut packetiser, 2, FrameKind::Delta, &payload) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        assert_eq!(queued(&mut session).len(), 1, "the first request");

        // The host refuses it and carries on sending delta frames. Five seconds
        // of them, which is longer than any interval the host's budget imposes.
        let mut asked = 0;
        let mut now = start;
        for id in 3..303u32 {
            now += Duration::from_millis(16);
            for datagram in picture(&mut packetiser, id, FrameKind::Delta, &payload) {
                session.datagram(&datagram, now).expect("a video packet");
            }
            asked += queued(&mut session).len();
        }
        assert!(
            asked > 0,
            "a request the host refused must be repeated, or the picture never returns"
        );
    }

    #[test]
    fn a_stuck_client_asks_again_even_when_the_host_sends_nothing() {
        // The gate on the host can shut the video path entirely, so a client
        // that only revisits the question when a datagram arrives never
        // revisits it at all. The deadline the receive loop already sleeps on is
        // what has to carry the retry.
        let opened = Instant::now();
        let mut session = Session::new(caps(), opened);
        let mut packetiser = bare_packetiser();
        let payload = payload();
        let start = opened + KEYFRAME_REQUEST_GAP;

        let delta = picture(&mut packetiser, 0, FrameKind::Delta, &payload);
        for datagram in &delta {
            session.datagram(datagram, start).expect("a video packet");
        }
        assert_eq!(
            session.video_stats().frames_undecodable,
            1,
            "a delta frame with nothing before it leaves the decoder stuck"
        );
        assert_eq!(queued(&mut session).len(), 1, "the first request");

        // Nothing else arrives, ever. The reassembler holds no open frame, so
        // its own deadline is gone, and what is left to wake the loop is the
        // report the arrivals owe the host and then the retry.
        let mut requests = 0;
        while let Some(deadline) = session.next_deadline() {
            if deadline > start + KEYFRAME_REQUEST_GAP {
                break;
            }
            session.expire(deadline);
            requests += queued(&mut session).len();
        }
        assert_eq!(requests, 1, "the retry the deadlines were for");
    }

    #[test]
    fn a_link_that_breaks_repeatedly_cannot_spam_the_host() {
        let opened = Instant::now();
        let mut session = Session::new(caps(), opened);
        let mut packetiser = bare_packetiser();
        let payload = payload();
        let start = opened + KEYFRAME_REQUEST_GAP;
        let mut id = 0;

        // The opening key frame arrives inside the gap the session opens with,
        // so it is waited for rather than asked about.
        for datagram in picture(&mut packetiser, id, FrameKind::Key, &payload) {
            session.datagram(&datagram, opened).expect("a video packet");
        }
        assert!(queued(&mut session).is_empty(), "nothing to ask about yet");
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
    fn every_datagram_that_arrives_is_reported_to_the_host() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        let mut packetiser = bare_packetiser();
        let payload = payload();

        let datagrams = picture(&mut packetiser, 0, FrameKind::Key, &payload);
        for (index, datagram) in datagrams.iter().enumerate() {
            session
                .datagram(datagram, start + Duration::from_micros(index as u64 * 250))
                .expect("a video packet");
        }
        // The first datagram is reported on its own, because the cadence has
        // nothing to hold it back yet; the rest wait for the interval.
        session.expire(start + FEEDBACK_INTERVAL);
        let reports = reports(&mut session);
        let described: usize = reports.iter().map(|report| report.packets.len()).sum();
        assert_eq!(
            described,
            datagrams.len(),
            "every datagram that arrived must be accounted for"
        );
        assert_eq!(reports[0].base, Seq(0));
        assert_eq!(reports[0].packets[0], PacketArrival::Received(0));
        assert_eq!(
            reports.last().expect("at least one").received,
            datagrams.len() as u64,
            "the cumulative counters are the reassembler's own"
        );
    }

    #[test]
    fn a_collapsing_link_is_reported_five_times_less_often() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        let mut packetiser = bare_packetiser();
        let payload = payload();
        session.host(
            HostMessage::Stream(StreamConfig {
                bitrate: NonZeroU32::new(FEEDBACK_SLOW_BELOW - 1).expect("nonzero"),
                ..stream()
            }),
            start,
            RTT,
        );

        let mut sent = 0;
        let mut id = 0;
        let mut now = start;
        while now < start + FEEDBACK_SLOW_INTERVAL {
            now += Duration::from_millis(10);
            for datagram in picture(&mut packetiser, id, FrameKind::Delta, &payload) {
                session.datagram(&datagram, now).expect("a video packet");
            }
            id += 1;
            sent += reports(&mut session).len();
        }
        assert_eq!(
            sent, 1,
            "under 250 kbit/s the report interval is 250 ms, not 50"
        );
    }

    #[test]
    fn a_still_screen_is_idle_and_not_a_lost_host() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        assert_eq!(session.liveness(start), Liveness::Idle);

        let mut packetiser = bare_packetiser();
        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload()) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        assert_eq!(session.liveness(start), Liveness::Streaming);

        // The measured worst case: the host's capture blocks for 1.53 seconds on
        // a screen nothing is drawing to, and keeps saying so on the control
        // stream throughout, because its heartbeat does not wait on capture.
        let still = Duration::from_millis(1_530);
        let beat = Heartbeat {
            sent: Timestamp::from_session_micros(1_000_000),
            last_frame: Some(FrameId(0)),
            frames_sent: 1,
            frames_gated: 0,
            frames_refused: 0,
            pacing_rate: None,
            estimate: None,
            padding_sent: 0,
            pipeline_micros: 4_200,
        };
        let mut at = start;
        while at < start + still {
            at += HEARTBEAT_INTERVAL;
            session.host(HostMessage::Heartbeat(beat), at, RTT);
        }
        assert_eq!(
            session.liveness(start + still),
            Liveness::Idle,
            "1.53 seconds without a frame is a still screen, not a dead host"
        );
        assert_eq!(session.heartbeat(), Some(beat));
    }

    /// The lie measured on the phone: 413 of 848 frames arrived whole and were
    /// thrown away for want of a key frame, and for every second of that the
    /// overlay said the screen was not changing. Liveness answers "is the
    /// screen changing", and a frame that arrived undecodable is a changing
    /// screen.
    #[test]
    fn a_stream_that_arrives_but_cannot_be_decoded_is_not_an_idle_screen() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        let mut packetiser = bare_packetiser();
        let payload = payload();

        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        // Frame one is lost whole; every complete delta frame after it is
        // undecodable until a key frame repairs the chain.
        let _ = picture(&mut packetiser, 1, FrameKind::Delta, &payload);
        let mut now = start;
        for id in 2..12u32 {
            now += Duration::from_millis(50);
            for datagram in picture(&mut packetiser, id, FrameKind::Delta, &payload) {
                session.datagram(&datagram, now).expect("a video packet");
            }
        }

        assert!(
            session.video_stats().frames_undecodable > 5,
            "the break must be live for this test to say anything"
        );
        assert_eq!(
            session.liveness(now),
            Liveness::Streaming,
            "ten frames in the last half second is not an idle screen"
        );
    }

    #[test]
    fn a_reordered_datagram_is_not_reported_as_loss() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        let mut packetiser = bare_packetiser();

        let mut out = picture(&mut packetiser, 0, FrameKind::Key, &payload());
        out.swap(2, 3);
        for (index, datagram) in out.iter().enumerate() {
            session
                .datagram(datagram, start + Duration::from_micros(index as u64 * 250))
                .expect("a video packet");
        }

        session.expire(start + FEEDBACK_INTERVAL);
        let last = reports(&mut session).pop().expect("arrivals owe a report");
        assert_eq!(
            last.lost, 0,
            "a packet that arrived out of order is not a loss the host may throttle for"
        );
        assert_eq!(last.received, out.len() as u64);
    }

    /// The radio in power save delivers packets in clumps a frame period
    /// apart. The wait for a damaged frame's repair must come from the
    /// stream's own frame rate, not from a constant sized for 60 Hz.
    #[test]
    fn a_repair_delayed_by_the_radio_is_waited_for_at_the_streams_own_rate() {
        let start = Instant::now();
        let mut session = Session::new(caps(), start);
        session.host(
            HostMessage::Stream(StreamConfig {
                framerate: NonZeroU16::new(20).expect("nonzero"),
                ..stream()
            }),
            start,
            RTT,
        );
        // The shipped parity level, because the delayed repair is the point.
        let mut packetiser = Packetiser::new(NonZeroUsize::new(1418).expect("nonzero"));
        let payload = payload();

        for datagram in picture(&mut packetiser, 0, FrameKind::Key, &payload) {
            session.datagram(&datagram, start).expect("a video packet");
        }
        assert!(session.pop_frame().is_some());

        // One picture datagram of frame one is really lost. The head of the
        // frame arrives, then the radio sleeps: the tail and the parity land
        // one frame period later.
        let mut damaged = picture(&mut packetiser, 1, FrameKind::Delta, &payload);
        assert!(damaged.len() >= 5, "picture and parity both");
        damaged.remove(1);
        for datagram in &damaged[..2] {
            session.datagram(datagram, start).expect("a video packet");
        }
        session.expire(start + Duration::from_millis(25));
        let later = start + Duration::from_millis(40);
        for datagram in &damaged[2..] {
            session.datagram(datagram, later).expect("a video packet");
        }
        session.expire(later + Duration::from_millis(51));

        let frame = session
            .pop_frame()
            .expect("the repair was in flight, not lost");
        assert_eq!(frame.id, FrameId(1));
        assert_eq!(session.video_stats().frames_recovered, 1);
        assert_eq!(session.video_stats().frames_dropped, 0);
        assert!(
            queued(&mut session).is_empty(),
            "nothing was lost, so nothing is asked for"
        );
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
