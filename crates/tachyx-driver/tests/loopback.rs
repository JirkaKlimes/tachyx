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
use tachyx_client::{Liveness, capabilities};
use tachyx_driver::{Error, FrameSink, Link, Stats};
use tachyx_proto::VERSION;
use tachyx_proto::control::{
    Aspect, ClientMessage, Codec, CodedSize, HEARTBEAT_INTERVAL, Heartbeat, HostMessage, Size,
    StreamConfig,
};
use tachyx_proto::input::{InputEvent, PointerButton, Press};
use tachyx_proto::media::{FrameId, FrameKind, Timestamp};
use tachyx_proto::packet::{AccessUnit, Packetiser};
use tachyx_transport::{ControlWriter, Host, HostIdentity, HostSession, Tuning};

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

/// A host on the loopback interface, running on a thread of its own.
///
/// The client blocks while it builds its runtime, so the host cannot share the
/// caller's.
struct Loopback {
    address: SocketAddr,
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
        Self::raw(name, |host, report| {
            Box::pin(async move {
                let session = host.accept().await.expect("the host accepts");
                play(session, report).await;
            })
        })
    }

    /// The same, but with the accepting left to the caller: a host that answers
    /// something no session would answer.
    fn raw<F>(name: &str, play: F) -> Self
    where
        F: FnOnce(Arc<Host>, Sender<String>) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
            + Send
            + 'static,
    {
        let directory =
            std::env::temp_dir().join(format!("tachyx-driver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let (bound, address) = channel();
        let (report, done) = channel();
        let listening = directory.clone();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let identity =
                    HostIdentity::load_or_create(&listening).expect("an identity on disk");
                let tuning = Tuning::for_stream(CodedSize::new(1920, 1088).expect("codeable"));
                let host = Arc::new(
                    Host::bind((Ipv4Addr::LOCALHOST, 0).into(), &identity, tuning)
                        .expect("a bound host"),
                );
                bound
                    .send(host.local_address().expect("a bound address"))
                    .expect("the test is waiting for the address");
                play(host, report).await;
            });
        });
        Self {
            address: address.recv().expect("the host binds"),
            directory,
            done,
            worker: Some(worker),
        }
    }

    /// Dials the host with a client that decodes into `decoder`.
    fn dial(&self, decoder: &Arc<Decoder>) -> Result<Link, Error> {
        let link = Link::connect(
            &self.address.to_string(),
            &self.directory,
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
            pacing_rate: host.video.pacing_rate(),
            pipeline_micros: 4_200,
        }))
        .await
        .expect("a live stream");

    let batch = host
        .from_client
        .recv::<ClientMessage>()
        .await
        .expect("the client speaks");
    report
        .send(format!("{batch:?}"))
        .expect("the test is waiting");
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

    // The user drags and clicks. Both reach the host in one batch, because the
    // driver ends a tick rather than sending an event.
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
    let batch = loopback.report();
    assert!(
        batch.contains("PointerMotion { dx: 16.0, dy: -8.0 }"),
        "eight samples of one motion arrive summed, and got {batch}"
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
                        pacing_rate: None,
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

            let asked = host
                .from_client
                .recv::<ClientMessage>()
                .await
                .expect("the client speaks");
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

    let stats = link.stats();
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
    let loopback = Loopback::raw("version", |host, report| {
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
    assert!(
        said.contains("protocol version 7"),
        "the client must be able to show this to a user, and got {said}"
    );
}

#[test]
fn a_refusal_reaches_the_client_as_the_close_reason() {
    const REASON: &str = "peer speaks protocol version 7, this build speaks 0";

    let loopback = Loopback::raw("refusal", |host, report| {
        Box::pin(async move {
            let incoming = host.endpoint().accept().await.expect("a client calls");
            let connection = incoming.await.expect("the handshake succeeds");
            let _stream = connection.accept_bi().await.expect("the stream opens");
            // The measured case: closing discards stream data the peer has not
            // acknowledged, so a refused client usually never sees the greeting
            // and reads the reason off the close instead.
            connection.close(quinn::VarInt::from_u32(1), REASON.as_bytes());
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
        said.contains(REASON),
        "the host's own words must reach the user, and got {said}"
    );
}
