//! The transport over a real socket.
//!
//! A real quinn host and a real quinn client on the loopback interface, moving
//! pictures of the sizes measured on the target machine: 130,427 bytes for a
//! 1080p key frame and 16,673 for a P frame at 8 Mbit/s and 60 Hz.
//!
//! Nothing here is a mock. The point of the file is that every claim the
//! transport makes about datagram accounting, backpressure and the handshake is
//! made against quinn's real behaviour rather than against a reading of its
//! source.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroU32};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tachyx_proto::VERSION;
use tachyx_proto::control::{
    Aspect, Capabilities, ClientHello, ClientMessage, Codec, CodedSize, HandshakeError, Heartbeat,
    HostMessage, InputBatch, Size, StreamConfig, TimedInput,
};
use tachyx_proto::input::{InputEvent, PointerButton, Press};
use tachyx_proto::media::{FrameId, FrameKind, HEADER_LEN, Timestamp};
use tachyx_proto::packet::{AccessUnit, Frame, Reassembler, ReassemblyStats};
use tachyx_transport::{
    Client, ClientSession, ControlError, ControlReader, ControlWriter, Host, HostIdentity,
    HostSession, PinStore, Sent, TransportError, Tuning, VideoSender, video_send_buffer,
};

/// How long the receiver waits for a frame that nothing follows.
///
/// Only the last frame before an idle screen needs it: every other incomplete
/// frame is decided by the arrival of the next one.
const FRAME_DEADLINE: Duration = Duration::from_millis(50);

/// The client side of the video path.
///
/// The transport carries datagrams and nothing more. Rebuilding pictures from
/// them is `tachyx_proto::packet`'s job, and the client's, so it is written out
/// here rather than offered by the transport: two implementations of it, one on
/// each side of the wire, is one too many.
struct Video {
    connection: quinn::Connection,
    reassembler: Reassembler,
}

impl Video {
    fn new(session: &ClientSession) -> Self {
        Self {
            connection: session.connection.clone(),
            reassembler: Reassembler::new(FRAME_DEADLINE),
        }
    }

    /// Waits for the next complete picture, giving up on an incomplete one when
    /// its deadline passes rather than waiting behind it.
    async fn next_frame(&mut self) -> Result<Frame, quinn::ConnectionError> {
        loop {
            // Popped before reading as well as after, because a picture can be
            // completed by parity rather than by the datagram that was pushed.
            if let Some(frame) = self.reassembler.pop() {
                return Ok(frame);
            }
            let datagram = match self.reassembler.next_deadline() {
                Some(at) => {
                    let read = self.connection.read_datagram();
                    let at = tokio::time::Instant::from_std(at);
                    let Ok(datagram) = tokio::time::timeout_at(at, read).await else {
                        self.reassembler.expire(Instant::now());
                        continue;
                    };
                    datagram?
                }
                None => self.connection.read_datagram().await?,
            };
            let _ = self.reassembler.push(&datagram, Instant::now());
        }
    }

    /// Reads until nothing has arrived for `patience`.
    ///
    /// A frame is delivered by its last picture datagram and its parity is sent
    /// behind it, so the tail of a stream is still in flight at the moment the
    /// last picture is complete. A test counting datagrams has to wait for it.
    async fn drain(&mut self, patience: Duration) {
        loop {
            let read = self.connection.read_datagram();
            let Ok(Ok(datagram)) = tokio::time::timeout(patience, read).await else {
                return;
            };
            let _ = self.reassembler.push(&datagram, Instant::now());
        }
    }

    fn stats(&self) -> ReassemblyStats {
        self.reassembler.stats()
    }

    fn needs_keyframe(&self) -> bool {
        self.reassembler.needs_keyframe()
    }
}

/// The name the host is pinned under. Not resolved, and it need not resolve.
const HOST_NAME: &str = "tachyx-host";

/// A 1080p60 key frame out of `vah264enc`, measured on the target machine.
const KEY_FRAME: usize = 130_427;

/// A P frame from the same stream at 8 Mbit/s and 60 Hz.
const DELTA_FRAME: usize = 16_673;

/// One frame period at 60 Hz.
const PERIOD: Duration = Duration::from_micros(16_667);

/// A host and a client bound on the loopback interface, with the host's key and
/// the client's pins in a directory of their own.
struct Link {
    host: Host,
    client: Client,
    address: SocketAddr,
    directory: PathBuf,
}

impl Link {
    fn open(name: &str, host: Tuning, client: Tuning) -> Self {
        let directory =
            std::env::temp_dir().join(format!("tachyx-loopback-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let identity = HostIdentity::load_or_create(&directory).expect("an identity on disk");
        let bound = Host::bind((Ipv4Addr::LOCALHOST, 0).into(), &identity, host).expect("a host");
        let address = bound.local_address().expect("a bound address");
        let pins = Arc::new(PinStore::open(directory.join("pins")).expect("a pin file"));
        Self {
            host: bound,
            client: Client::new(pins, client).expect("a client endpoint"),
            address,
            directory,
        }
    }

    /// Both ends of one session.
    ///
    /// Joined rather than spawned: the client writes its hello before the host
    /// sees the stream at all, so the two futures have to be driven together
    /// even on a single-threaded runtime.
    async fn sessions(&self) -> (HostSession, ClientSession) {
        let hello = ClientHello::new(capabilities());
        let (host, client) = tokio::join!(
            self.host.accept(),
            self.client.connect(self.address, HOST_NAME, &hello)
        );
        (
            host.expect("the host accepts"),
            client.expect("the client connects"),
        )
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        viewport: Size::new(2400, 1080).expect("a size"),
        max_decode: Size::new(1920, 1088).expect("a size"),
        max_decode_fps: NonZeroU16::new(60).expect("nonzero"),
        requested_fps: NonZeroU16::new(60).expect("nonzero"),
        bitrate_ceiling: NonZeroU32::new(20_000_000).expect("nonzero"),
    }
}

/// Bytes that depend on the frame identifier, so the receiver can check a frame
/// against what was sent without the two sides sharing anything.
fn picture(id: u32, len: usize) -> Vec<u8> {
    let mut state = u64::from(id).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

/// Key frame at the start of every group of pictures, delta frames after it.
fn kind(id: u32, key_interval: u32) -> FrameKind {
    if id.is_multiple_of(key_interval) {
        FrameKind::Key
    } else {
        FrameKind::Delta
    }
}

fn frame_len(id: u32, key_interval: u32) -> usize {
    match kind(id, key_interval) {
        FrameKind::Key => KEY_FRAME,
        FrameKind::Delta => DELTA_FRAME,
    }
}

/// Sends one picture of the measured size for its position in the group.
fn send_frame(video: &mut VideoSender, id: u32, key_interval: u32) -> Sent {
    let picture = picture(id, frame_len(id, key_interval));
    let slices: [&[u8]; 1] = [&picture];
    let unit = AccessUnit::new(
        FrameId(id),
        Timestamp::from_session_micros(u64::from(id) * 16_667),
        kind(id, key_interval),
        &slices,
    )
    .expect("a well formed picture");
    video.send(&unit).expect("the connection is live")
}

/// What the receiving end saw.
#[derive(Debug, Default)]
struct Report {
    frames: u32,
    bytes: usize,
    latency: Vec<Duration>,
    stats: ReassemblyStats,
}

async fn receive(
    session: ClientSession,
    frames: u32,
    key_interval: u32,
    start: tokio::time::Instant,
) -> Report {
    let mut video = Video::new(&session);
    let mut report = Report::default();
    for _ in 0..frames {
        let next = tokio::time::timeout(Duration::from_secs(2), video.next_frame());
        let Ok(frame) = next.await else { break };
        let frame = frame.expect("the connection is live");
        assert_eq!(
            frame.data,
            picture(frame.id.0, frame_len(frame.id.0, key_interval)),
            "frame {} came back wrong",
            frame.id.0
        );
        let due = start + PERIOD * frame.id.0;
        report.latency.push(tokio::time::Instant::now() - due);
        report.frames += 1;
        report.bytes += frame.data.len();
    }
    video.drain(Duration::from_millis(200)).await;
    report.stats = video.stats();
    report
}

/// The value at `fraction` of the way through a sample.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "an index into a sample of at most a few hundred measurements"
    )]
    let index = ((sorted.len() as f64 - 1.0) * fraction) as usize;
    sorted[index]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_moves_over_a_real_socket_at_the_measured_frame_sizes() {
    const FRAMES: u32 = 180;
    const KEY_INTERVAL: u32 = 60;

    let coded = CodedSize::new(1920, 1088).expect("a codeable size");
    let tuning = Tuning::for_stream(coded);
    let link = Link::open("video", tuning, tuning);
    let (mut host, client) = link.sessions().await;
    let payload = host.video.payload_len().expect("the handshake has settled");
    let start = tokio::time::Instant::now();
    let receiving = tokio::spawn(receive(client, FRAMES, KEY_INTERVAL, start));

    let mut datagrams = 0usize;
    let mut bytes = 0usize;
    for id in 0..FRAMES {
        tokio::time::sleep_until(start + PERIOD * id).await;
        match send_frame(&mut host.video, id, KEY_INTERVAL) {
            Sent::Queued { datagrams: sent } => {
                datagrams += sent;
                bytes += frame_len(id, KEY_INTERVAL);
            }
            Sent::Refused => panic!("frame {id} was refused on an idle loopback link"),
        }
    }
    let sending = start.elapsed();
    let report = receiving.await.expect("the receiver did not panic");
    let mut latency = report.latency.clone();
    latency.sort_unstable();
    let quinn = host.connection.stats();

    #[expect(
        clippy::cast_precision_loss,
        reason = "a report of megabytes and milliseconds, not an accumulator"
    )]
    let megabits = (bytes as f64 * 8.0) / 1_000_000.0;
    println!(
        "\n  frames             {}/{FRAMES} delivered, {} datagrams, {payload}-byte payloads\n  \
           carried            {megabits:.2} Mbit in {:.3} s = {:.2} Mbit/s\n  \
           capture->delivery  median {:?}, p99 {:?}, worst {:?}\n  \
           round trip         {:?} (quinn path estimate)\n  \
           pacing rate        {:?} bit/s\n  \
           target bitrate     {:?} bit/s\n  \
           quinn path         {} udp datagrams sent, {} lost packets, {} congestion events\n  \
           reassembly         {:?}\n",
        report.frames,
        datagrams,
        sending.as_secs_f64(),
        megabits / sending.as_secs_f64(),
        percentile(&latency, 0.5),
        percentile(&latency, 0.99),
        latency.last().expect("frames arrived"),
        host.connection.rtt(),
        host.video.pacing_rate(),
        host.video.target_bitrate(),
        quinn.udp_tx.datagrams,
        quinn.path.lost_packets,
        quinn.path.congestion_events,
        report.stats,
    );

    assert_eq!(report.frames, FRAMES, "every frame must arrive");
    assert_eq!(
        report.stats.packets_received, datagrams as u64,
        "every datagram the gate accepted must reach the receiver"
    );
    assert_eq!(
        report.stats.packets_lost, 0,
        "nothing was evicted and nothing was lost"
    );
    assert_eq!(report.stats.frames_dropped, 0);
    assert_eq!(report.stats.frames_missing, 0);
    assert_eq!(report.stats.frames_undecodable, 0);
    assert_eq!(host.video.frames_refused(), 0);
    assert!(
        host.video.pacing_rate().is_some(),
        "BBR is configured, so there is a bandwidth estimate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_link_reports_what_it_carried() {
    const KEY_INTERVAL: u32 = 60;
    const RUN: Duration = Duration::from_millis(1500);

    let coded = CodedSize::new(1920, 1088).expect("a codeable size");
    let tuning = Tuning::for_stream(coded);
    let link = Link::open("saturate", tuning, tuning);
    let (mut host, client) = link.sessions().await;

    let receiving = tokio::spawn(async move {
        let mut video = Video::new(&client);
        let mut frames = 0u64;
        let mut bytes = 0usize;
        while let Ok(Ok(frame)) =
            tokio::time::timeout(Duration::from_millis(300), video.next_frame()).await
        {
            assert_eq!(
                frame.data,
                picture(frame.id.0, frame_len(frame.id.0, KEY_INTERVAL)),
                "frame {} came back wrong under saturation",
                frame.id.0
            );
            frames += 1;
            bytes += frame.data.len();
        }
        (frames, bytes, video.stats())
    });

    let start = tokio::time::Instant::now();
    let mut offered = 0u32;
    let mut bytes = 0usize;
    while start.elapsed() < RUN {
        if let Sent::Queued { .. } = send_frame(&mut host.video, offered, KEY_INTERVAL) {
            bytes += frame_len(offered, KEY_INTERVAL);
        }
        offered += 1;
        // The gate refuses on a full queue, and a refusal costs nothing, so a
        // tight loop would spin. This yields to the connection instead.
        tokio::task::yield_now().await;
    }
    let elapsed = start.elapsed();
    drop(host.to_client);
    let (frames, received, stats) = receiving.await.expect("the receiver did not panic");

    #[expect(
        clippy::cast_precision_loss,
        reason = "a report of megabits, not an accumulator"
    )]
    let rate = (bytes as f64 * 8.0) / 1_000_000.0 / elapsed.as_secs_f64();
    println!(
        "\n  offered            {offered} frames, {} queued, {} refused by the gate\n  \
           carried            {rate:.1} Mbit/s over {:.3} s\n  \
           delivered          {frames} frames, {received} bytes\n  \
           pacing rate        {:?} bit/s\n  \
           round trip         {:?}\n  \
           reassembly         {stats:?}\n",
        host.video.frames_sent(),
        host.video.frames_refused(),
        elapsed.as_secs_f64(),
        host.video.pacing_rate(),
        host.connection.rtt(),
    );

    assert!(host.video.frames_sent() > 0, "the link carried something");
    assert!(
        host.video.pacing_rate().is_some(),
        "BBR is configured, so there is a bandwidth estimate"
    );
}

/// A buffer that cannot hold a frame the transport is offered.
fn cramped() -> Tuning {
    Tuning {
        video_buffer: video_send_buffer(CodedSize::new(640, 480).expect("a codeable size")),
    }
}

#[tokio::test]
async fn the_gate_refuses_a_frame_the_queue_cannot_hold_whole() {
    let link = Link::open("gate", cramped(), Tuning::for_stream(hd()));
    let (mut host, _client) = link.sessions().await;

    assert_eq!(
        send_frame(&mut host.video, 0, 1),
        Sent::Refused,
        "a key frame is twenty times the queue this host was given"
    );
    assert_eq!(host.video.frames_refused(), 1);
    assert_eq!(host.video.frames_sent(), 0);
    assert!(!host.video.can_send_bytes(KEY_FRAME));
    assert!(
        host.video.can_send_bytes(1),
        "the queue is small, not closed"
    );
}

fn hd() -> CodedSize {
    CodedSize::new(1920, 1088).expect("a codeable size")
}

#[tokio::test]
async fn the_gate_charges_the_queue_accounting_once_per_queued_datagram() {
    let tuning = Tuning::for_stream(hd());
    let link = Link::open("accounting", tuning, tuning);
    let (mut host, _client) = link.sessions().await;
    let payload = host.video.payload_len().expect("the handshake has settled");
    let overhead = size_of::<bytes::Bytes>();
    let charged = payload.get() + HEADER_LEN + overhead;

    // A single-threaded runtime and nothing awaited from here on, so nothing
    // drains the queue while it is being measured. This is the state a real
    // key frame leaves behind: ninety-two entries queued at once.
    let Sent::Queued { datagrams } = send_frame(&mut host.video, 0, 1) else {
        panic!("the queue holds a key frame")
    };
    let space = host.connection.datagram_send_buffer_space();

    // What quinn's report alone would have allowed, and what the queue can
    // really take: the difference is the accounting of the datagrams already
    // in it, which the report leaves out and the eviction loop charges.
    let reported = space / charged;
    let real = (space + overhead - datagrams * overhead) / charged;
    println!(
        "\n  queued             {datagrams} datagrams, {space} bytes reported free\n  \
           report allows      {reported} more\n  \
           queue really takes {real} more\n"
    );

    assert!(
        real < reported,
        "with {datagrams} datagrams queued the two accountings must differ"
    );
    assert!(host.video.can_send_frame(real, payload.get()));
    assert!(
        !host.video.can_send_frame(real + 1, payload.get()),
        "one datagram past what the queue holds is refused, though the report \
         alone would have allowed {reported}"
    );
}

/// Reads datagrams until none arrives for `quiet`.
async fn drain(session: &ClientSession, quiet: Duration) -> usize {
    let mut arrived = 0;
    while tokio::time::timeout(quiet, session.connection.read_datagram())
        .await
        .is_ok_and(|datagram| datagram.is_ok())
    {
        arrived += 1;
    }
    arrived
}

#[tokio::test]
async fn a_receive_buffer_smaller_than_a_key_frame_shreds_it() {
    let link = Link::open("cramped-receive", Tuning::for_stream(hd()), cramped());
    let (mut host, client) = link.sessions().await;
    let payload = host.video.payload_len().expect("the handshake has settled");

    let queued = send_frame(&mut host.video, 0, 1);
    let Sent::Queued { datagrams } = queued else {
        panic!("the host's own queue holds a key frame")
    };
    let arrived = drain(&client, Duration::from_millis(300)).await;
    println!(
        "\n  receive buffer     {} bytes, key frame {} datagrams of {payload} bytes\n  \
           arrived            {arrived}\n",
        cramped().video_buffer,
        datagrams,
    );
    assert!(
        arrived < datagrams,
        "a receive buffer this small has to discard, or the concern is imaginary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_configured_buffers_hold_a_key_frame_the_client_is_slow_to_read() {
    let tuning = Tuning::for_stream(hd());
    let link = Link::open("slow-reader", tuning, tuning);
    let (mut host, client) = link.sessions().await;
    let mut video = Video::new(&client);

    let Sent::Queued { datagrams } = send_frame(&mut host.video, 0, 1) else {
        panic!("the queue holds a key frame")
    };
    // The receiving application is busy: nothing reads the datagrams while the
    // whole key frame arrives and queues up on the client.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let frame = tokio::time::timeout(Duration::from_secs(2), video.next_frame())
        .await
        .expect("the frame arrives")
        .expect("the connection is live");
    assert_eq!(frame.data, picture(0, KEY_FRAME));
    assert_eq!(frame.kind, FrameKind::Key);
    video.drain(Duration::from_millis(200)).await;
    assert_eq!(video.stats().packets_received, datagrams as u64);
    assert_eq!(video.stats().packets_lost, 0);
    assert!(!video.needs_keyframe());
    println!(
        "\n  buffers            {} bytes each end\n  \
           key frame          {datagrams} datagrams held for 100 ms, none discarded\n",
        tuning.video_buffer,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_matching_version_negotiates_and_carries_input_and_heartbeats() {
    let tuning = Tuning::for_stream(hd());
    let link = Link::open("control", tuning, tuning);
    let (mut host, mut client) = link.sessions().await;

    assert_eq!(client.greeting.version(), VERSION);
    assert_eq!(host.hello.version(), VERSION);
    assert_eq!(host.hello.capabilities, capabilities());

    let stream = HostMessage::Stream(StreamConfig {
        codec: Codec::H264,
        coded: hd(),
        visible: Size::new(1920, 1080).expect("a size"),
        display_aspect: Aspect::new(16, 9).expect("a ratio"),
        framerate: NonZeroU16::new(60).expect("nonzero"),
        bitrate: NonZeroU32::new(8_000_000).expect("nonzero"),
    });
    let heartbeat = HostMessage::Heartbeat(Heartbeat {
        sent: Timestamp::from_session_micros(1_234_500),
        last_frame: Some(FrameId(41)),
        frames_sent: 42,
        frames_gated: 1,
        frames_refused: 0,
        pacing_rate: host.video.pacing_rate(),
        estimate: Some(8_000_000),
        padding_sent: 0,
        pipeline_micros: 7300,
    });
    let input = ClientMessage::Input(InputBatch {
        events: vec![
            TimedInput {
                at: Timestamp::from_session_micros(500),
                event: InputEvent::PointerMotion { dx: 3.0, dy: -1.5 },
            },
            TimedInput {
                at: Timestamp::from_session_micros(1500),
                event: InputEvent::PointerButton {
                    button: PointerButton::Left,
                    press: Press::Down,
                },
            },
        ],
    });
    let request = ClientMessage::KeyframeRequest {
        after: Some(FrameId(41)),
    };

    host.to_client.send(&stream).await.expect("a live stream");
    host.to_client
        .send(&heartbeat)
        .await
        .expect("a live stream");
    client.to_host.send(&input).await.expect("a live stream");
    client.to_host.send(&request).await.expect("a live stream");

    assert_eq!(
        client.from_host.recv::<HostMessage>().await.expect("read"),
        stream
    );
    assert_eq!(
        client.from_host.recv::<HostMessage>().await.expect("read"),
        heartbeat
    );
    assert_eq!(
        host.from_client
            .recv::<ClientMessage>()
            .await
            .expect("read"),
        input
    );
    assert_eq!(
        host.from_client
            .recv::<ClientMessage>()
            .await
            .expect("read"),
        request
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_of_another_version_is_refused_by_name() {
    // A hello this build cannot construct: `ClientHello` keeps its version
    // private precisely so that no client can claim a version it does not
    // speak. A future build reshapes the fields after the version, so the
    // stranger below writes different ones.
    #[derive(serde::Serialize)]
    struct Later {
        version: u16,
        whatever: [u32; 3],
    }

    let tuning = Tuning::for_stream(hd());
    let link = Link::open("mismatch", tuning, tuning);
    let stranger = async {
        let connection = link
            .client
            .endpoint()
            .connect(link.address, HOST_NAME)
            .expect("a dialable address")
            .await
            .expect("the handshake succeeds: the version is not a TLS matter");
        let (send, recv) = connection.open_bi().await.expect("the stream opens");
        let mut writer = ControlWriter::new(send);
        writer
            .send(&Later {
                version: VERSION + 7,
                whatever: [1, 2, 3],
            })
            .await
            .expect("the write lands");
        // The host greets before it reads, but closing a connection discards
        // stream data the peer has not acknowledged, so a refused client
        // usually loses the race for the greeting and learns the reason from
        // the close instead. Either is a named answer; neither is silence.
        let greeting: Result<tachyx_proto::control::HostHello, ControlError> =
            ControlReader::new(recv).recv_hello().await;
        if let Ok(greeting) = greeting {
            assert_eq!(greeting.version(), VERSION);
        }
        connection.closed().await
    };

    let (refused, closed) = tokio::join!(link.host.accept(), stranger);
    let refused = refused.expect_err("a hello from another version is not a session");
    assert!(
        matches!(
            refused,
            TransportError::Control(ControlError::Handshake(HandshakeError::Version { theirs }))
                if theirs == VERSION + 7
        ),
        "the mismatch must be named, not misparsed: {refused}"
    );
    let reason = closed.to_string();
    println!("\n  host refused       {refused}\n  client saw         {reason}\n");
    assert!(
        reason.contains("protocol version"),
        "the client is told why the connection closed, and got {reason}"
    );
}

/// Frames one control message the way [`ControlWriter`] does, so a test can put
/// the length and the body on the wire separately.
fn framed<M: serde::Serialize>(message: &M) -> (Vec<u8>, Vec<u8>) {
    let body = postcard::to_stdvec(message).expect("a message that encodes");
    let header = u32::try_from(body.len()).expect("a short message");
    (header.to_be_bytes().to_vec(), body)
}

/// A raw client, so the test owns the send stream rather than a `ControlWriter`.
///
/// The hello is written by hand in the same framing the writer uses, because
/// what is being tested is a message arriving in two pieces and the writer has
/// no way to say that.
async fn raw_session(
    link: &Link,
) -> (
    HostSession,
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
) {
    let dial = async {
        let connection = link
            .client
            .endpoint()
            .connect(link.address, HOST_NAME)
            .expect("a dialable address")
            .await
            .expect("the handshake succeeds");
        let (mut send, recv) = connection.open_bi().await.expect("the stream opens");
        let (header, body) = framed(&ClientHello::new(capabilities()));
        send.write_all(&header).await.expect("the write lands");
        send.write_all(&body).await.expect("the write lands");
        (connection, send, recv)
    };
    let (host, (connection, send, recv)) = tokio::join!(link.host.accept(), dial);
    (host.expect("the host accepts"), connection, send, recv)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_in_a_select_survives_a_message_that_arrives_in_two_pieces() {
    // The daemon reads the control stream as one arm of a `select!` beside two
    // timers. Every tick therefore drops a half-finished read, and a reader that
    // took its bytes off the stream and then lost them leaves the framing cut in
    // the wrong place: the next length is read out of the middle of a message
    // and never squares again. Input stops for the rest of the session while the
    // connection stays up, because keepalives and heartbeats are on other paths.
    let tuning = Tuning::for_stream(hd());
    let link = Link::open("cancel", tuning, tuning);
    // The connection and the reading half are held for the test's whole life:
    // dropping either closes what is being measured.
    let (mut host, _connection, mut send, _recv) = raw_session(&link).await;

    let message = ClientMessage::KeyframeRequest {
        after: Some(FrameId(41)),
    };
    let (header, body) = framed(&message);
    send.write_all(&header).await.expect("the write lands");

    // The body follows far enough behind that the reader is cancelled many
    // times between the two.
    let write = async {
        tokio::time::sleep(Duration::from_millis(60)).await;
        send.write_all(&body).await.expect("the write lands");
    };
    let read = async {
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                message = host.from_client.recv::<ClientMessage>() => break message,
                _ = tick.tick() => {}
            }
        }
    };

    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let (received, ()) = tokio::join!(read, write);
        received
    })
    .await
    .expect("a reader in a select must not lose the framing");
    assert_eq!(
        received.expect("the message survives the cancellations"),
        message
    );
}

/// Long enough to tell one picture's datagrams from the next picture's, and
/// far longer than any picture here is paced over.
const BETWEEN_PICTURES: Duration = Duration::from_millis(200);

/// Splits a run of arrivals into one group per picture, at the pauses between
/// them.
fn pictures(arrivals: &[Instant]) -> Vec<Vec<Instant>> {
    let mut groups: Vec<Vec<Instant>> = Vec::new();
    for &at in arrivals {
        match groups.last_mut() {
            Some(last)
                if at.duration_since(*last.last().expect("a group is never empty"))
                    < BETWEEN_PICTURES / 2 =>
            {
                last.push(at);
            }
            _ => groups.push(vec![at]),
        }
    }
    groups
}

/// First arrival to last, which is what the receiver's frame deadline is
/// measured over.
fn span(arrivals: &[Instant]) -> Duration {
    match (arrivals.first(), arrivals.last()) {
        (Some(first), Some(last)) => last.duration_since(*first),
        _ => Duration::ZERO,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_frame_leaves_paced_and_a_delta_frame_leaves_at_once() {
    let tuning = Tuning::for_stream(hd());
    let link = Link::open("pacing", tuning, tuning);
    let (host, client) = link.sessions().await;
    let mut video = host.video;
    let control = host.control;

    // Every datagram of the session, stamped as it arrives. Reading them raw
    // rather than through the reassembler is the point: what is being checked
    // is when they leave, not that they add up to a picture.
    let receiving = tokio::spawn(async move {
        let mut arrivals = Vec::new();
        while tokio::time::timeout(BETWEEN_PICTURES * 2, client.connection.read_datagram())
            .await
            .is_ok_and(|datagram| datagram.is_ok())
        {
            arrivals.push(Instant::now());
        }
        arrivals
    });

    // On a thread of its own, because `send` blocks until the pacer has
    // released the whole picture. This is the shape the daemon's pump uses.
    let sending = tokio::task::spawn_blocking(move || {
        let mut sent = Vec::new();
        // What the session configures the encoder for. The pacing rate is
        // derived from it and from the key frame the sender has measured.
        control.set_target_bitrate(NonZeroU32::new(8_000_000).expect("nonzero"));
        let paced = video.release_rate();
        // A key frame, then a delta frame, then the same key frame again with
        // the pacer set so far above what is being sent that it never engages,
        // which is what the transport did before it had one.
        for (id, key_interval) in [(0, 1), (1, 60), (2, 1)] {
            if id == 2 {
                control.set_target_bitrate(NonZeroU32::MAX);
            }
            let started = Instant::now();
            let Sent::Queued { datagrams } = send_frame(&mut video, id, key_interval) else {
                panic!("frame {id} was refused on an idle loopback link")
            };
            sent.push((datagrams, started.elapsed()));
            std::thread::sleep(BETWEEN_PICTURES);
        }
        (paced, video.release_rate(), sent)
    });

    let (paced, unpaced, sent) = sending.await.expect("the sender did not panic");
    let arrivals = receiving.await.expect("the receiver did not panic");
    let groups = pictures(&arrivals);
    assert_eq!(groups.len(), 3, "three pictures were sent");
    let [key, delta, burst] = [&groups[0], &groups[1], &groups[2]];

    println!(
        "\n  paced at           {paced} bit/s, and {unpaced} bit/s for the burst\n  \
           key frame          {} datagrams, {:?} to leave, arriving over {:?}\n  \
           delta frame        {} datagrams, {:?} to leave, arriving over {:?}\n  \
           key frame unpaced  {} datagrams, {:?} to leave, arriving over {:?}\n",
        sent[0].0,
        sent[0].1,
        span(key),
        sent[1].0,
        sent[1].1,
        span(delta),
        sent[2].0,
        sent[2].1,
        span(burst),
    );

    for (index, (datagrams, _)) in sent.iter().enumerate() {
        assert_eq!(
            groups[index].len(),
            *datagrams,
            "every datagram of picture {index} reached the client"
        );
    }
    assert!(
        span(key) > Duration::from_millis(5),
        "a key frame is released over a span, not at once: {:?}",
        span(key)
    );
    assert!(
        span(key) < FRAME_DEADLINE,
        "a paced key frame still has to arrive inside the receiver's deadline: {:?}",
        span(key)
    );
    assert!(
        span(delta) < Duration::from_millis(2),
        "a delta frame fits the burst allowance and is not paced at all: {:?}",
        span(delta)
    );
    assert!(
        span(burst) < span(key) / 4,
        "with the pacer set above what is being sent the same key frame goes \
         out as a burst: {:?} against {:?}",
        span(burst),
        span(key)
    );
}
