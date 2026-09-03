//! The whole congestion loop over a real socket and a shaped path.
//!
//! Nothing here is a mock except the encoder. A quinn host binds the loopback
//! interface, a userspace relay in front of it gives the path a chosen one-way
//! delay and a chosen bottleneck rate with a finite drop-tail queue, and the
//! real [`Link`] dials the relay. The packetiser, the reassembler, the client's
//! receiver report and the host's estimator are all the shipped ones.
//!
//! The relay is the shaper from the pacing research in the scratchpad, trimmed
//! to the three things this test varies. Its queue is fluid rather than per
//! packet: a datagram offered at `now` goes on the wire at `max(next_free,
//! now)` and occupies `bytes * 8 / rate` of it, so the backlog is
//! `next_free - now` and a datagram offered when that already exceeds the
//! queue's depth is dropped. That is what a per-station queue inside an access
//! point does.
//!
//! What it proves is the thing `spec/congestion.md` is for: the host finds a
//! link nobody told it about, gives it up when it collapses, and takes it back.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tachyx_client::capabilities;
use tachyx_congestion::{Controller, Update};
use tachyx_driver::{FrameSink, Link};
use tachyx_proto::control::{
    Aspect, ClientMessage, Codec, CodedSize, HEARTBEAT_INTERVAL, Heartbeat, HostMessage, Size,
    StreamConfig,
};
use tachyx_proto::media::{FrameId, FrameKind, Timestamp};
use tachyx_proto::packet::{AccessUnit, Parity};
use tachyx_transport::endpoint::HostSession;
use tachyx_crypto::store::PairedClient;
use tachyx_crypto::{DeviceId, DeviceKey, PairedClients, Permissions};
use tachyx_transport::{Host, Tuning};
use tokio::net::UdpSocket;
use tokio::time::Instant;

/// The frame interval the synthetic encoder produces at.
const FRAME: Duration = Duration::from_micros(16_667);

/// One key frame every two seconds, which is what the daemon's own budget
/// allows at these rates.
const KEY_INTERVAL: u32 = 120;

/// A key frame against a delta frame, as measured on the target machine:
/// 130,427 bytes against 16,673 at 8 Mbit/s and 60 Hz.
const KEY_MULTIPLE: u64 = 8;

/// Half the 13.91 ms round trip measured to the phone.
const DELAY: Duration = Duration::from_micros(6_955);

/// Bytes the bottleneck holds before it drops the tail. 128 kB is the depth at
/// which a burst of the measured key frame stopped being destroyed.
const QUEUE: u64 = 128_000;

/// A count as a double, which every figure here is far below the exact integer
/// range of.
#[expect(
    clippy::cast_precision_loss,
    reason = "a byte count and a bitrate are both far below two to the fifty-third"
)]
fn counted(count: u64) -> f64 {
    count as f64
}

/// The surface the client says it has.
fn surface() -> Size {
    Size::new(1920, 1080).expect("a size")
}

/// A path whose capacity changes under the sender.
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// Bits a second the bottleneck carries to begin with.
    rate: u64,
    /// Rates it changes to, and how long after the relay started.
    changes: &'static [(Duration, u64)],
}

/// One direction of the shaped path.
struct Lane {
    shape: Shape,
    started: Instant,
    next_free: Instant,
    out: VecDeque<(Instant, Vec<u8>)>,
}

impl Lane {
    fn new(shape: Shape, now: Instant) -> Self {
        Self {
            shape,
            started: now,
            next_free: now,
            out: VecDeque::new(),
        }
    }

    fn rate(&self, now: Instant) -> u64 {
        let elapsed = now.duration_since(self.started);
        self.shape
            .changes
            .iter()
            .filter(|(after, _)| *after <= elapsed)
            .map(|(_, rate)| *rate)
            .next_back()
            .unwrap_or(self.shape.rate)
    }

    fn offer(&mut self, packet: &[u8], now: Instant) {
        let rate = counted(self.rate(now));
        let bytes = counted(u64::try_from(packet.len()).unwrap_or(0));
        let start = self.next_free.max(now);
        // The backlog is how long the wire is already busy for, in bytes at the
        // rate it carries them.
        let backlog = (start - now).as_secs_f64() * rate / 8.0;
        if backlog + bytes > counted(QUEUE) {
            return;
        }
        self.next_free = start + Duration::from_secs_f64(bytes * 8.0 / rate);
        self.out
            .push_back((self.next_free + DELAY, packet.to_vec()));
    }

    fn due(&self) -> Option<Instant> {
        self.out.front().map(|(at, _)| *at)
    }
}

/// Runs a relay in front of `host` and returns the address a client dials.
async fn relay(host: SocketAddr, down: Shape) -> std::io::Result<SocketAddr> {
    let front = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = front.local_addr()?;
    let back = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;

    tokio::spawn(async move {
        let now = Instant::now();
        // Only the path the video takes is shaped. The return path carries the
        // receiver reports, and a report delayed is a measurement the estimator
        // reads correctly anyway: every quantity in it is a difference between
        // two of the client's own arrival times.
        let mut up = Lane::new(
            Shape {
                rate: 1_000_000_000,
                changes: &[],
            },
            now,
        );
        let mut down = Lane::new(down, now);
        let mut client: Option<SocketAddr> = None;
        let mut from_front = vec![0u8; 2048];
        let mut from_back = vec![0u8; 2048];
        loop {
            let wake = match (up.due(), down.due()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (one, None) | (None, one) => one,
            };
            let sleep = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                got = front.recv_from(&mut from_front) => {
                    let Ok((len, from)) = got else { continue };
                    client = Some(from);
                    up.offer(&from_front[..len], Instant::now());
                }
                got = back.recv_from(&mut from_back) => {
                    let Ok((len, _)) = got else { continue };
                    down.offer(&from_back[..len], Instant::now());
                }
                () = sleep => {
                    let now = Instant::now();
                    while up.due().is_some_and(|at| at <= now) {
                        let (_, packet) = up.out.pop_front().expect("the front is due");
                        let _ = back.send_to(&packet, host).await;
                    }
                    while down.due().is_some_and(|at| at <= now) {
                        let (_, packet) = down.out.pop_front().expect("the front is due");
                        if let Some(to) = client {
                            let _ = front.send_to(&packet, to).await;
                        }
                    }
                }
            }
        }
    });
    Ok(address)
}

/// What the client handed the decoder.
#[derive(Debug, Default)]
struct Decoder {
    frames: Mutex<u64>,
}

impl FrameSink for Decoder {
    fn push(&self, _access_unit: Vec<u8>) {
        *self.frames.lock().expect("no panic held it") += 1;
    }
}

/// The estimate, sampled while the session ran.
type Samples = Vec<(Duration, u32)>;

/// A synthetic encoder: a token bucket over the target, charged the size of
/// every frame it lets through, exactly as `tachyx_media` charges the real one.
struct Encoder {
    balance: f64,
    refilled: Instant,
    next: u32,
}

impl Encoder {
    fn new(now: Instant) -> Self {
        Self {
            balance: 0.0,
            refilled: now,
            next: 0,
        }
    }

    /// The next picture, or `None` while the bucket is in debt.
    fn frame(&mut self, target: u32, now: Instant) -> Option<(FrameId, FrameKind, Vec<u8>)> {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        self.balance += f64::from(target) * elapsed;
        self.balance = self.balance.min(f64::from(target) * 0.1);
        if self.balance < 0.0 {
            return None;
        }
        let id = self.next;
        self.next += 1;
        let kind = if id.is_multiple_of(KEY_INTERVAL) {
            FrameKind::Key
        } else {
            FrameKind::Delta
        };
        let mut bytes = u64::from(target) / 60 / 8;
        if matches!(kind, FrameKind::Key) {
            bytes *= KEY_MULTIPLE;
        }
        let bytes = bytes.max(64);
        self.balance -= counted(bytes) * 8.0;
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        Some((FrameId(id), kind, vec![0x5A; bytes]))
    }
}

/// The send path, on a thread of its own because `send` blocks until the pacer
/// has released the whole picture.
fn produce(
    mut video: tachyx_transport::video::VideoSender,
    target: &Arc<AtomicU32>,
    stop: &Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let target = Arc::clone(target);
    let stop = Arc::clone(stop);
    tokio::task::spawn_blocking(move || {
        let mut encoder = Encoder::new(Instant::now());
        let mut due = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < due {
                std::thread::sleep((due - now).min(FRAME));
                continue;
            }
            due += FRAME;
            let held = target.load(Ordering::Relaxed);
            if let Some((id, kind, bytes)) = encoder.frame(held, now) {
                let slices: [&[u8]; 1] = [&bytes];
                let unit = AccessUnit::new(id, Timestamp::from_session_micros(0), kind, &slices)
                    .expect("one non-empty slice is a picture");
                if video.send(&unit).is_err() {
                    break;
                }
            // No picture to be the cluster, which is what padding is for.
            } else if video.pad().is_err() {
                break;
            }
        }
    })
}

/// Runs the host half of the session: a controller, a synthetic encoder, and
/// the shipped send path.
async fn serve(session: HostSession, seconds: u64, report: &Sender<Samples>) {
    let HostSession {
        connection,
        hello,
        video,
        control,
        mut to_client,
        mut from_client,
        ..
    } = session;
    let started = Instant::now();
    let mut controller = Controller::new(hello.capabilities.bitrate_ceiling, started.into_std());
    let mut parity = Parity::default();
    let mut samples: Samples = Vec::new();

    to_client
        .send(&HostMessage::Stream(StreamConfig {
            codec: Codec::H264,
            coded: CodedSize::new(1920, 1088).expect("a codeable size"),
            visible: surface(),
            display_aspect: Aspect::new(16, 9).expect("a ratio"),
            framerate: NonZeroU16::new(60).expect("nonzero"),
            bitrate: controller.video_target(),
        }))
        .await
        .expect("the client is listening");

    let target = Arc::new(AtomicU32::new(controller.video_target().get()));
    let stop = Arc::new(AtomicBool::new(false));
    let pump = produce(video, &target, &stop);

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let deadline = started + Duration::from_secs(seconds);
    let spend = |update: Update, controller: &Controller| {
        control.set_target_bitrate(update.video);
        target.store(update.video.get(), Ordering::Relaxed);
        if let Some(probe) = update.probe {
            control.probe(probe);
        }
        let _ = controller;
    };

    while Instant::now() < deadline {
        tokio::select! {
            message = from_client.recv::<ClientMessage>() => {
                let Ok(ClientMessage::Feedback(feedback)) = message else {
                    if message.is_err() { break }
                    continue;
                };
                for release in control.released() {
                    controller.released(&release);
                }
                let update = controller.feedback(&feedback, std::time::Instant::now());
                parity.observe(update.losses.lost, update.losses.received);
                control.set_parity(parity.level());
                spend(update, &controller);
            }
            _ = tick.tick() => {
                for release in control.released() {
                    controller.released(&release);
                }
                controller.set_rtt(connection.rtt());
                let update = controller.tick(std::time::Instant::now());
                spend(update, &controller);
                samples.push((started.elapsed(), controller.video_target().get()));
            }
            _ = beat.tick() => {
                let alive = to_client.send(&HostMessage::Heartbeat(Heartbeat {
                    sent: Timestamp::from_session_micros(0),
                    last_frame: None,
                    frames_sent: 0,
                    frames_gated: 0,
                    frames_refused: 0,
                    pacing_rate: None,
                    estimate: Some(controller.estimate().get()),
                    padding_sent: 0,
                    pipeline_micros: 0,
                })).await;
                if alive.is_err() {
                    break;
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = pump.await;
    report.send(samples).expect("the test is waiting");
}

/// The estimate in force at `at` seconds into the run.
fn at(samples: &Samples, at: f64) -> u32 {
    samples
        .iter()
        .filter(|(when, _)| when.as_secs_f64() <= at)
        .map(|(_, rate)| *rate)
        .next_back()
        .unwrap_or(0)
}

/// Runs one shaped session and returns what the host's estimate did.
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

fn run(name: &str, shape: Shape, seconds: u64) -> (Samples, u64) {
    let directory =
        std::env::temp_dir().join(format!("tachyx-shaped-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let (bound, front) = channel();
    let (report, done) = channel();
    let (host_key, host_device, clients) = paired(&directory);

    let host = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async move {
            let tuning = Tuning::for_stream(CodedSize::new(1920, 1088).expect("codeable"));
            let host = Host::bind((Ipv4Addr::LOCALHOST, 0).into(), &host_key, clients, tuning)
                .expect("a bound host");
            let address = relay(host.local_address().expect("a bound address"), shape)
                .await
                .expect("a relay");
            bound.send(address).expect("the test is waiting");
            let session = host.accept().await.expect("the host accepts");
            serve(session, seconds, &report).await;
        });
    });

    let front: SocketAddr = front.recv().expect("the host binds");
    let decoder = Arc::new(Decoder::default());
    let link = Link::dial(
        &front.to_string(),
        host_device,
        &directory.join("client"),
        capabilities(surface(), NonZeroU16::new(60).expect("nonzero")),
    )
    .expect("the host accepts");
    link.present_on(Arc::clone(&decoder) as Arc<dyn FrameSink>);

    let samples = done
        .recv_timeout(Duration::from_secs(seconds + 20))
        .expect("the host reported before the test gave up");
    drop(link);
    let _ = host.join();
    let _ = std::fs::remove_dir_all(&directory);
    let frames = *decoder.frames.lock().expect("no panic held it");
    (samples, frames)
}

/// Twelve megabits, cut to three at six seconds, restored at fourteen.
const CLIFF: [(Duration, u64); 2] = [
    (Duration::from_secs(6), 3_000_000),
    (Duration::from_secs(14), 12_000_000),
];

/// One session, one shaped path, and the three things the whole design is for.
///
/// It is one test rather than three because two shaped sessions running at once
/// compete for the processor, and a delay-based estimator reads the scheduler's
/// jitter as a queue. The controller's behaviour on each leg is asserted
/// separately below; what is shared is the twenty-two seconds of real socket.
#[test]
fn the_host_finds_a_link_gives_it_up_and_takes_it_back() {
    let (samples, frames) = run(
        "cliff",
        Shape {
            rate: 12_000_000,
            changes: &CLIFF,
        },
        22,
    );

    // Found. Without the receiver report there is nothing to find it with:
    // quinn's own estimate was measured reporting 0.00 Mbit/s on a healthy
    // hundred-megabit path for every sample of a twenty-second run.
    let found = at(&samples, 5.5);
    assert!(
        found > 5_000_000,
        "a twelve megabit link left the host asking for {found}: {samples:?}"
    );

    // Given up. The arrival spacing in the reports is the only thing that says
    // the path changed; nothing else on the host can see it.
    let cut = at(&samples, 13.0);
    assert!(
        cut < 4_500_000,
        "the host was still asking for {cut} on a three megabit link: {samples:?}"
    );

    // Taken back. Multiplicative increase alone would take ln(4)/ln(1.08), or
    // eighteen seconds, to climb from three megabits to twelve; this has eight.
    let back = at(&samples, 21.0);
    assert!(
        back > 5_000_000,
        "the host never took the recovered link back, and sat at {back}: {samples:?}"
    );

    assert!(frames > 200, "only {frames} pictures reached the decoder");
}
