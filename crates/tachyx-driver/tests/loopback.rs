//! The client against a real host over a real socket.
//!
//! Nothing here is a mock. A quinn host binds the loopback interface, the real
//! [`Link`] dials it, and pictures, input, heartbeats and a key frame request
//! all cross the wire. The host side is written out by hand rather than taken
//! from the daemon, so the test states what the protocol requires of a host
//! rather than what one implementation happens to do.
//!
//! The two ends run on separate threads with separate runtimes, because
//! [`Link::connect`] blocks while it builds its own.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroU32};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tachyx_client::{AudioEvent, Liveness, capabilities};
use tachyx_driver::{AudioSink, Error, FrameSink, Link, Stats};
use tachyx_proto::VERSION;
use tachyx_proto::control::{
    Aspect, AudioCommand, AudioMode, AudioStatus, ClientMessage, Codec, CodedSize,
    HEARTBEAT_INTERVAL, Heartbeat, HostMessage, Size, StreamConfig,
};
use tachyx_proto::input::{InputEvent, PointerButton, Press};
use tachyx_proto::media::{FrameId, FrameKind, Timestamp};
use tachyx_proto::packet::{AccessUnit, Packetiser, Parity};
use tachyx_crypto::store::PairedClient;
use tachyx_crypto::{DeviceId, DeviceKey, PairedClients, Permissions};
use tachyx_transport::{ControlWriter, Host, HostSession, Tuning};

/// A host key, its identifier, and a paired list holding the client's device.
///
/// The two devices get directories of their own inside `state`, because a device
/// key is one file with one name and a host and a client that shared a directory
/// would share an identity.
fn paired(state: &std::path::Path) -> (DeviceKey, DeviceId, Arc<PairedClients>) {
    let host = DeviceKey::load_or_create(&state.join("host")).expect("a host key");
    let client = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = Arc::new(PairedClients::open(state.join("clients.toml")).expect("a paired list"));
    clients
        .add(PairedClient {
            device: client.id(),
            name: "the test client".to_owned(),
            permissions: Permissions::default(),
            paired: 0,
        })
        .expect("the list is writable");
    let device = host.id();
    (host, device, clients)
}

/// A 1080p60 key frame out of `vah264enc`, measured on the target machine.
const KEY_FRAME: usize = 130_427;

/// A P frame from the same stream at 8 Mbit/s and 60 Hz.
const DELTA_FRAME: usize = 16_673;

/// Long enough for a loopback round trip and a scheduler that is busy, short
/// enough that a broken test fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(5);

/// The surface the client says it has.
fn surface() -> Size {
    Size::new(1920, 1080).expect("a size")
}

fn stream() -> StreamConfig {
    StreamConfig {
        codec: Codec::H264,
        coded: CodedSize::new(1920, 1088).expect("a codeable size"),
        visible: surface(),
        display_aspect: Aspect::new(16, 9).expect("a ratio"),
        framerate: NonZeroU16::new(60).expect("nonzero"),
        bitrate: NonZeroU32::new(8_000_000).expect("nonzero"),
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

/// What the driver handed the decoder.
#[derive(Debug, Default)]
struct Decoder {
    units: Mutex<Vec<Vec<u8>>>,
}

impl Decoder {
    fn units(&self) -> Vec<Vec<u8>> {
        self.units.lock().expect("no panic held it").clone()
    }
}

impl FrameSink for Decoder {
    fn push(&self, access_unit: Vec<u8>) {
        self.units
            .lock()
            .expect("no panic held it")
            .push(access_unit);
    }
}

/// What the driver handed the speakers.
#[derive(Debug, Default)]
struct Ears {
    heard: Mutex<Vec<AudioEvent>>,
}

impl Ears {
    fn heard(&self) -> Vec<AudioEvent> {
        self.heard.lock().expect("no panic held it").clone()
    }
}

impl AudioSink for Ears {
    fn push(&self, event: AudioEvent) {
        self.heard.lock().expect("no panic held it").push(event);
    }
}

/// A host on the loopback interface, running on a thread of its own.
///
/// The client blocks while it builds its runtime, so the host cannot share the
/// caller's.
struct Loopback {
    address: SocketAddr,
    host_device: DeviceId,
    directory: PathBuf,
    done: Receiver<String>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Loopback {
    /// Runs `play` as the host, with a directory of its own for the key it
    /// presents and the pin the client takes of it.
    fn open<F>(name: &str, play: F) -> Self
    where
        F: FnOnce(HostSession, Sender<String>) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
            + Send
            + 'static,
    {
        Self::accepting(name, false, play)
    }

    /// The same, for a host whose hello says it can stream its own sound. The
    /// client offers its audio controls from that and nothing else.
    fn listening<F>(name: &str, play: F) -> Self
    where
        F: FnOnce(HostSession, Sender<String>) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
            + Send
            + 'static,
    {
        Self::accepting(name, true, play)
    }

    fn accepting<F>(name: &str, audio: bool, play: F) -> Self
    where
        F: FnOnce(HostSession, Sender<String>) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
            + Send
            + 'static,
    {
        Self::raw(name, audio, |host, report| {
            Box::pin(async move {
                let session = host.accept().await.expect("the host accepts");
                play(session, report).await;
            })
        })
    }

    /// The same, but with the accepting left to the caller: a host that answers
    /// something no session would answer.
    fn raw<F>(name: &str, audio: bool, play: F) -> Self
    where
        F: FnOnce(Arc<Host>, Sender<String>) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
            + Send
            + 'static,
    {
        let directory =
            std::env::temp_dir().join(format!("tachyx-driver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let (host_key, host_device, clients) = paired(&directory);
        let (bound, address) = channel();
        let (report, done) = channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let tuning = Tuning::for_stream(CodedSize::new(1920, 1088).expect("codeable"));
                let host = Arc::new(
                    Host::bind((Ipv4Addr::LOCALHOST, 0).into(), &host_key, clients, tuning)
                        .expect("a bound host")
                        .with_audio(audio),
                );
                bound
                    .send(host.local_address().expect("a bound address"))
                    .expect("the test is waiting for the address");
                play(host, report).await;
            });
        });
        Self {
            address: address.recv().expect("the host binds"),
            host_device,
            directory,
            done,
            worker: Some(worker),
        }
    }

    /// Dials the host with a client that decodes into `decoder`.
    fn dial(&self, decoder: &Arc<Decoder>) -> Result<Link, Error> {
        let link = Link::dial(
            &self.address.to_string(),
            self.host_device,
            &self.directory.join("client"),
            capabilities(surface(), NonZeroU16::new(60).expect("nonzero")),
        )?;
        link.present_on(Arc::clone(decoder) as Arc<dyn FrameSink>);
        Ok(link)
    }

    /// What the host reported, once it has.
    fn report(&self) -> String {
        self.done
            .recv_timeout(PATIENCE)
            .expect("the host reported before the test gave up")
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Waits until `ready` holds of the statistics, or gives up.
fn until(link: &Link, ready: impl Fn(&Stats) -> bool) -> Stats {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let stats = link.stats();
        if ready(&stats) || Instant::now() > deadline {
            return stats;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// One picture, as an access unit of the size its kind implies.
fn unit(id: u32, kind: FrameKind) -> Vec<u8> {
    picture(
        id,
        match kind {
            FrameKind::Key => KEY_FRAME,
            FrameKind::Delta => DELTA_FRAME,
        },
    )
}

fn kind(id: u32) -> FrameKind {
    if id.is_multiple_of(10) {
        FrameKind::Key
    } else {
        FrameKind::Delta
    }
}

/// How many pictures the session test moves.
const FRAMES: u32 = 20;

/// A host that streams, says what it has been doing, and reads what the user
/// did.
/// The next thing the client says that these tests are about.
///
/// Receiver reports are not: a datagram that arrives is a datagram whose
/// arrival has to be reported, so every test that sends a picture would
/// otherwise have to step around one. What the reports themselves say is
/// asserted where the reporter lives.
async fn next_of_interest(from_client: &mut tachyx_transport::ControlReader) -> ClientMessage {
    loop {
        let message = from_client
            .recv::<ClientMessage>()
            .await
            .expect("the client speaks");
        if !matches!(message, ClientMessage::Feedback(_)) {
            return message;
        }
    }
}

async fn stream_and_listen(mut host: HostSession, report: Sender<String>) {
    host.to_client
        .send(&HostMessage::Stream(stream()))
        .await
        .expect("a live stream");
    for id in 0..FRAMES {
        let payload = unit(id, kind(id));
        let slices: [&[u8]; 1] = [&payload];
        let picture = AccessUnit::new(
            FrameId(id),
            Timestamp::from_session_micros(u64::from(id) * 16_667),
            kind(id),
            &slices,
        )
        .expect("a well formed picture");
        assert!(
            matches!(
                host.video.send(&picture).expect("the connection is live"),
                tachyx_transport::Sent::Queued { .. }
            ),
            "frame {id} was refused on an idle loopback link"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    host.to_client
        .send(&HostMessage::Heartbeat(Heartbeat {
            sent: Timestamp::from_session_micros(400_000),
            last_frame: Some(FrameId(FRAMES - 1)),
            frames_sent: u64::from(FRAMES),
            frames_gated: 0,
            frames_refused: 0,
            pacing_rate: host.video.pacing_rate(),
            estimate: Some(8_000_000),
            padding_sent: 0,
            pipeline_micros: 4_200,
        }))
        .await
        .expect("a live stream");

    // Two, because the first move of a quiet period travels on its own and the
    // tick collects what follows it.
    for _ in 0..2 {
        let batch = next_of_interest(&mut host.from_client).await;
        report
            .send(format!("{batch:?}"))
            .expect("the test is waiting");
    }
    // Held open until the client leaves, so the departure is a close and not a
    // reset.
    host.connection.closed().await;
}

#[test]
fn video_input_and_liveness_all_cross_a_real_socket() {
    let loopback = Loopback::open("session", |host, report| {
        Box::pin(stream_and_listen(host, report))
    });

    let decoder = Arc::new(Decoder::default());
    let link = loopback.dial(&decoder).expect("the host accepts");

    let stats = until(&link, |stats| {
        stats.video.frames_delivered == u64::from(FRAMES)
    });
    assert_eq!(
        stats.video.frames_delivered,
        u64::from(FRAMES),
        "every frame the host queued must reach the decoder: {stats:?}"
    );
    assert_eq!(stats.video.frames_dropped, 0);
    assert_eq!(stats.video.frames_missing, 0);
    assert_eq!(stats.video.packets_lost, 0);
    assert_eq!(stats.stream, Some(stream()), "the geometry reached the app");
    assert_eq!(stats.liveness, Liveness::Streaming);
    assert!(
        stats.rtt > Duration::ZERO,
        "quinn measures the path itself, so there is a round trip to report"
    );

    let units = decoder.units();
    assert_eq!(units.len(), FRAMES as usize);
    for (id, shown) in units.iter().enumerate() {
        let id = u32::try_from(id).expect("a small count");
        assert_eq!(*shown, unit(id, kind(id)), "frame {id} came back wrong");
    }

    // The user drags and clicks. The first sample of the drag leaves at once,
    // because the pointer had been still and no batch was waiting for it to
    // join. Everything the tick then collects reaches the host together, click
    // included, because the driver ends a tick rather than sending an event.
    for step in 1..=8u64 {
        link.input(
            Timestamp::from_session_micros(step * 1_000),
            InputEvent::PointerMotion { dx: 2.0, dy: -1.0 },
        );
    }
    link.input(
        Timestamp::from_session_micros(9_000),
        InputEvent::PointerButton {
            button: PointerButton::Left,
            press: Press::Down,
        },
    );
    let first = loopback.report();
    assert!(
        first.contains("PointerMotion { dx: 2.0, dy: -1.0 }"),
        "the first sample does not wait out a tick to be alone, and got {first}"
    );
    let batch = loopback.report();
    assert!(
        batch.contains("PointerMotion { dx: 14.0, dy: -7.0 }"),
        "the seven samples behind it arrive summed, and got {batch}"
    );
    assert!(
        batch.contains("Left"),
        "the click is a boundary, not something to merge into the motion: {batch}"
    );

    let beating = until(&link, |stats| stats.host.is_some());
    let beat = beating.host.expect("the host said what it had been doing");
    assert_eq!(beat.frames_sent, u64::from(FRAMES));
    assert_eq!(beat.last_frame, Some(FrameId(FRAMES - 1)));
}

#[test]
fn a_still_screen_is_idle_rather_than_a_lost_host() {
    let loopback = Loopback::open("idle", |host, report| {
        Box::pin(async move {
            // Taken whole. A block that touched only the fields it uses would
            // capture only those, and dropping the reading half of the control
            // stream sends `STOP_SENDING`, which makes the client's next write
            // fail. A real host holds the session.
            let mut host = host;
            host.to_client
                .send(&HostMessage::Stream(stream()))
                .await
                .expect("a live stream");
            // The capture blocks on a screen nothing is drawing to, for gaps
            // measured up to 1.53 seconds. The heartbeat is the only thing that
            // separates that from a host that has died.
            for beat in 0..6u32 {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                host.to_client
                    .send(&HostMessage::Heartbeat(Heartbeat {
                        sent: Timestamp::from_session_micros(u64::from(beat) * 500_000),
                        last_frame: None,
                        frames_sent: 0,
                        frames_gated: 0,
                        frames_refused: 0,
                        pacing_rate: None,
                        estimate: None,
                        padding_sent: 0,
                        pipeline_micros: 0,
                    }))
                    .await
                    .expect("a live stream");
            }
            report.send("beat".to_owned()).expect("the test is waiting");
            host.connection.closed().await;
        })
    });

    let decoder = Arc::new(Decoder::default());
    let link = loopback.dial(&decoder).expect("the host accepts");
    assert_eq!(loopback.report(), "beat");

    let stats = link.stats();
    assert_eq!(
        stats.liveness,
        Liveness::Idle,
        "three seconds of heartbeats and no video is a still screen: {stats:?}"
    );
    assert_eq!(stats.video.frames_delivered, 0);
    assert!(stats.ended.is_none(), "an idle host has not gone anywhere");
}

#[test]
fn a_frame_the_wire_ate_earns_one_key_frame_request() {
    let loopback = Loopback::open("recovery", |mut host, report| {
        Box::pin(async move {
            host.to_client
                .send(&HostMessage::Stream(stream()))
                .await
                .expect("a live stream");
            let payload = host.video.payload_len().expect("the handshake has settled");
            let mut packetiser = Packetiser::new(payload);
            // No parity, because what is under test is the request a hole
            // earns. With parity this hole is repaired and nothing is asked
            // for, which is the point of parity and not of this test.
            packetiser.set_parity(Parity::fixed(0));

            // Sent by hand rather than through the sender, because what is under
            // test is a datagram that never arrives, and the sender has no way
            // to send all but one of them.
            let mut send = |id: u32, kind: FrameKind, hold_back: usize| {
                let payload = unit(id, kind);
                let slices: [&[u8]; 1] = [&payload];
                let picture = AccessUnit::new(
                    FrameId(id),
                    Timestamp::from_session_micros(u64::from(id) * 16_667),
                    kind,
                    &slices,
                )
                .expect("a well formed picture");
                let mut datagrams = Vec::new();
                packetiser.packetise(&picture, &mut datagrams);
                datagrams.truncate(datagrams.len() - hold_back);
                for datagram in datagrams {
                    host.connection
                        .send_datagram(Bytes::from(datagram))
                        .expect("the connection is live");
                }
            };
            send(0, FrameKind::Key, 0);
            send(1, FrameKind::Delta, 1);
            send(2, FrameKind::Delta, 0);

            let asked = next_of_interest(&mut host.from_client).await;
            report
                .send(format!("{asked:?}"))
                .expect("the test is waiting");
            host.connection.closed().await;
        })
    });

    let decoder = Arc::new(Decoder::default());
    let link = loopback.dial(&decoder).expect("the host accepts");
    let asked = loopback.report();
    assert_eq!(
        asked, "KeyframeRequest { after: Some(FrameId(0)) }",
        "one request, naming the last picture that did decode"
    );

    // Waited for rather than read: the loop publishes on its own cadence, so
    // the host having the request is no proof the handle has the count behind
    // it yet.
    let stats = until(&link, |stats| {
        stats.video.frames_delivered == 1
            && stats.video.frames_dropped == 1
            && stats.video.frames_undecodable == 1
    });
    assert_eq!(
        stats.video.frames_delivered, 1,
        "only the key frame was decodable: {stats:?}"
    );
    assert_eq!(
        stats.video.frames_dropped, 1,
        "the damaged frame was given up"
    );
    assert_eq!(
        stats.video.frames_undecodable, 1,
        "the frame after it arrived whole and still could not be shown"
    );
}

/// A hello from a build that reshaped everything after the version.
#[derive(serde::Serialize)]
struct Later {
    version: u16,
    whatever: [u32; 3],
}

#[test]
fn a_host_of_another_version_is_named_and_not_misparsed() {
    let loopback = Loopback::raw("version", false, |host, report| {
        Box::pin(async move {
            let incoming = host.endpoint().accept().await.expect("a client calls");
            let connection = incoming.await.expect("the handshake succeeds");
            let (send, _recv) = connection.accept_bi().await.expect("the stream opens");
            ControlWriter::new(send)
                .send(&Later {
                    version: VERSION + 7,
                    whatever: [1, 2, 3],
                })
                .await
                .expect("the write lands");
            report.send("greeted".to_owned()).expect("the test waits");
            connection.closed().await;
        })
    });

    let decoder = Arc::new(Decoder::default());
    let refused = loopback
        .dial(&decoder)
        .expect_err("a host of another version is not a session");
    let said = refused.to_string();
    let theirs = VERSION + 7;
    assert!(
        said.contains(&format!("protocol version {theirs}")),
        "the client must be able to show this to a user, and got {said}"
    );
}

#[test]
fn a_refusal_reaches_the_client_as_the_close_reason() {
    let reason = format!(
        "peer speaks protocol version {}, this build speaks {VERSION}",
        VERSION + 7
    );

    let loopback = Loopback::raw("refusal", false, |host, report| {
        Box::pin(async move {
            let incoming = host.endpoint().accept().await.expect("a client calls");
            let connection = incoming.await.expect("the handshake succeeds");
            let _stream = connection.accept_bi().await.expect("the stream opens");
            // The measured case: closing discards stream data the peer has not
            // acknowledged, so a refused client usually never sees the greeting
            // and reads the reason off the close instead.
            let spoken = format!(
                "peer speaks protocol version {}, this build speaks {VERSION}",
                VERSION + 7
            );
            connection.close(quinn::VarInt::from_u32(1), spoken.as_bytes());
            // Held until quinn has put the closing frame on the wire. Letting
            // the runtime die here instead would take the socket with it and the
            // client would learn only that the host had gone quiet.
            host.endpoint().wait_idle().await;
            report.send("refused".to_owned()).expect("the test waits");
        })
    });

    let decoder = Arc::new(Decoder::default());
    let refused = loopback
        .dial(&decoder)
        .expect_err("a host that closes is not a session");
    let said = refused.to_string();
    assert!(
        said.contains(&reason),
        "the host's own words must reach the user, and got {said}"
    );
}

/// How many packets of sound the audio test moves.
const SOUNDS: u32 = 10;

/// The one packet the wire is told to eat, so that the hole is a hole in the
/// identifiers and nothing else. It has packets on both sides of it, because a
/// receiver reads a hole from the sound that arrives after it.
const EATEN: u32 = 5;

/// What the host says about its sound before anything is streamed.
fn status() -> AudioStatus {
    AudioStatus {
        mode: AudioMode::Shared,
        volume: 650,
        muted: false,
    }
}

/// One Opus packet, as far as anything under test can tell: nothing in the
/// client decodes it, so what matters is that these bytes and no others come
/// out the other end.
fn sound(id: u32) -> Vec<u8> {
    picture(id, 200)
}

/// The next audio command the client sends.
///
/// Everything else is stepped around rather than asserted on: a session that
/// has been open longer than a key frame request gap asks for a key frame, and
/// which side of that this test lands on is the scheduler's business.
async fn next_command(from_client: &mut tachyx_transport::ControlReader) -> AudioCommand {
    loop {
        if let ClientMessage::Audio(command) = from_client
            .recv::<ClientMessage>()
            .await
            .expect("the client speaks")
        {
            return command;
        }
    }
}

async fn stream_sound(mut host: HostSession, report: Sender<String>) {
    // What the host is doing about sound, sent as soon as the session opens.
    host.to_client
        .send(&HostMessage::Audio(status()))
        .await
        .expect("a live stream");

    // Nothing is captured until the user asks for it, which is what the mode
    // command is. Waiting for it here is the protocol and not a contrivance:
    // sound sent before anybody asked would be sound played by nothing.
    let asked = next_command(&mut host.from_client).await;
    report
        .send(format!("{asked:?}"))
        .expect("the test is waiting");

    for id in 0..SOUNDS {
        if id == EATEN {
            continue;
        }
        host.audio
            .send(
                FrameId(id),
                Timestamp::from_session_micros(u64::from(id) * 10_000),
                &sound(id),
            )
            .expect("the connection is live");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    host.connection.closed().await;
}

#[test]
fn sound_input_and_the_volume_control_all_cross_a_real_socket() {
    let loopback = Loopback::listening("audio", |host, report| {
        Box::pin(stream_sound(host, report))
    });

    let decoder = Arc::new(Decoder::default());
    let ears = Arc::new(Ears::default());
    let link = loopback.dial(&decoder).expect("the host accepts");
    // Both go through the loop's own queue, in this order, so the sink is in
    // place before the host is asked for a single packet.
    link.listen_on(Arc::clone(&ears) as Arc<dyn AudioSink>);
    link.set_audio_mode(AudioMode::Exclusive);
    assert_eq!(
        loopback.report(),
        "SetMode(Exclusive)",
        "the command has to reach the host, and only a host that said it has \
         sound is sent one"
    );

    let stats = until(&link, |stats| {
        stats.audio_packets == u64::from(SOUNDS) - 1 && stats.audio.is_some()
    });
    assert!(
        stats.host_audio,
        "the host's hello said it has sound, and nothing else answers that: {stats:?}"
    );
    assert_eq!(stats.audio, Some(status()), "the status reached the app");
    assert_eq!(stats.audio_packets, u64::from(SOUNDS) - 1);
    assert_eq!(
        stats.audio_lost, 1,
        "the packet that was never sent is a hole in the sound"
    );

    // Every packet, in the order it was captured, with the hole where the wire
    // ate one and the sound behind it played after the hole rather than in
    // place of it.
    let heard = ears.heard();
    let mut expected: Vec<AudioEvent> = (0..SOUNDS)
        .filter(|id| *id != EATEN)
        .map(|id| AudioEvent::Packet {
            id: FrameId(id),
            capture: Timestamp::from_session_micros(u64::from(id) * 10_000),
            opus: sound(id),
        })
        .collect();
    expected.insert(EATEN as usize, AudioEvent::Lost(1));
    assert_eq!(heard, expected, "the sound came back out of order or short");
}
