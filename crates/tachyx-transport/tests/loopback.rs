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
use std::time::Duration;

use tachyx_proto::VERSION;
use tachyx_proto::control::{
    Aspect, Capabilities, ClientHello, ClientMessage, Codec, CodedSize, HandshakeError, Heartbeat,
    HostMessage, InputBatch, Size, StreamConfig, TimedInput,
};
use tachyx_proto::input::{InputEvent, PointerButton, Press};
use tachyx_proto::media::{FrameId, FrameKind, HEADER_LEN, Timestamp};
use tachyx_proto::packet::{AccessUnit, ReassemblyStats};
use tachyx_transport::{
    Client, ClientSession, ControlError, ControlReader, ControlWriter, Host, HostIdentity,
    HostSession, PinStore, Sent, TransportError, Tuning, video_send_buffer,
};

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
fn send_frame(session: &mut HostSession, id: u32, key_interval: u32) -> Sent {
    let picture = picture(id, frame_len(id, key_interval));
    let slices: [&[u8]; 1] = [&picture];
    let unit = AccessUnit::new(
        FrameId(id),
        Timestamp::from_session_micros(u64::from(id) * 16_667),
        kind(id, key_interval),
        &slices,
    )
    .expect("a well formed picture");
    session.video.send(&unit).expect("the connection is live")
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
    mut session: ClientSession,
    frames: u32,
    key_interval: u32,
    start: tokio::time::Instant,
) -> Report {
    let mut report = Report::default();
    for _ in 0..frames {
        let next = tokio::time::timeout(Duration::from_secs(2), session.video.next_frame());
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
    report.stats = session.video.stats();
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
        match send_frame(&mut host, id, KEY_INTERVAL) {
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
    let (mut host, mut client) = link.sessions().await;

    let receiving = tokio::spawn(async move {
        let mut frames = 0u64;
        let mut bytes = 0usize;
        while let Ok(Ok(frame)) =
            tokio::time::timeout(Duration::from_millis(300), client.video.next_frame()).await
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
        (frames, bytes, client.video.stats())
    });

    let start = tokio::time::Instant::now();
    let mut offered = 0u32;
    let mut bytes = 0usize;
    while start.elapsed() < RUN {
        if let Sent::Queued { .. } = send_frame(&mut host, offered, KEY_INTERVAL) {
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
        frame_deadline: Duration::from_millis(50),
    }
}

#[tokio::test]
async fn the_gate_refuses_a_frame_the_queue_cannot_hold_whole() {
    let link = Link::open("gate", cramped(), Tuning::for_stream(hd()));
    let (mut host, _client) = link.sessions().await;

    assert_eq!(
        send_frame(&mut host, 0, 1),
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
    let Sent::Queued { datagrams } = send_frame(&mut host, 0, 1) else {
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

    let queued = send_frame(&mut host, 0, 1);
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
    let (mut host, mut client) = link.sessions().await;

    let Sent::Queued { datagrams } = send_frame(&mut host, 0, 1) else {
        panic!("the queue holds a key frame")
    };
    // The receiving application is busy: nothing reads the datagrams while the
    // whole key frame arrives and queues up on the client.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let frame = tokio::time::timeout(Duration::from_secs(2), client.video.next_frame())
        .await
        .expect("the frame arrives")
        .expect("the connection is live");
    assert_eq!(frame.data, picture(0, KEY_FRAME));
    assert_eq!(frame.kind, FrameKind::Key);
    assert_eq!(client.video.stats().packets_received, datagrams as u64);
    assert_eq!(client.video.stats().packets_lost, 0);
    assert!(!client.video.needs_keyframe());
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
        pacing_rate: host.video.pacing_rate(),
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
