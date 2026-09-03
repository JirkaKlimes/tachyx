//! Pairs with a running host and watches the stream, or reconnects to one it
//! has already paired with, and reports what arrived.
//!
//! Run the daemons first, then either
//!
//! `cargo run -p tachyx-driver --example connect -- pair <CODE> <RELAY>`
//!
//! with the code `tachyx-hostd pair` is showing and the relay as
//! `address:port#KEY`, or
//!
//! `cargo run -p tachyx-driver --example connect -- session <DEVICE> [RELAY]`
//!
//! for a host this probe has paired with before. Either way it opens a real
//! session, counts the pictures, and prints what the client measured, which is
//! what makes "the phone shows a black screen" attributable: a probe that
//! counts frames on this machine separates the host's half of the pipeline
//! from the phone's decoder.
//!
//! The probe prints its own device identifier, because that is what ends up in
//! the host's paired list and `tachyx-hostd pairings revoke` takes it back out.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tachyx_crypto::{DeviceId, DeviceKey, RelayAddress};
use tachyx_driver::{FrameSink, Link, Paired};
use tachyx_proto::control::{Capabilities, Codec, Codecs, Size};

#[derive(Debug, Default)]
struct Counter {
    frames: AtomicU64,
    bytes: AtomicU64,
    first: std::sync::Mutex<Option<Instant>>,
}

impl FrameSink for Counter {
    fn push(&self, bytes: Vec<u8>) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let mut first = self.first.lock().expect("counter mutex");
        if first.is_none() {
            *first = Some(Instant::now());
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let seconds: u64 = std::env::args()
        .nth(4)
        .and_then(|value| value.parse().ok())
        .unwrap_or(6);

    let directory = std::env::temp_dir().join("tachyx-connect-probe");
    std::fs::create_dir_all(&directory)?;
    println!("this probe is {}", DeviceKey::load_or_create(&directory)?.id());

    let (host, relays) = match mode.as_str() {
        "pair" => {
            let code = std::env::args().nth(2).ok_or("pair takes the code")?;
            let relay: RelayAddress = std::env::args()
                .nth(3)
                .ok_or("pair takes the relay as address:port#KEY")?
                .parse()?;
            let relays = vec![relay];
            let paired = Paired::run(&code, &relays, &directory, "the connect probe")?;
            println!(
                "paired with {} ({}), both screens show {}",
                paired.host.name, paired.host.device, paired.words
            );
            (paired.host.device, relays)
        }
        "session" => {
            let host = DeviceId::parse(
                &std::env::args()
                    .nth(2)
                    .ok_or("session takes the host's device identifier")?,
            )?;
            let relays = match std::env::args().nth(3) {
                Some(relay) => vec![relay.parse()?],
                None => Vec::new(),
            };
            (host, relays)
        }
        _ => return Err("the first argument is `pair` or `session`".into()),
    };

    let capabilities = Capabilities {
        viewport: Size::new(2400, 1080).expect("a viewport"),
        max_decode: Size::new(3840, 2160).expect("a decode ceiling"),
        max_decode_fps: NonZeroU16::new(120).expect("a frame rate"),
        requested_fps: NonZeroU16::new(60).expect("a frame rate"),
        bitrate_ceiling: NonZeroU32::new(20_000_000).expect("a bitrate"),
        // This probe counts bytes and decodes nothing, so it asks for what a
        // client on this host would have to decode.
        decode: Codecs::of(Codec::H264),
    };

    let started = Instant::now();
    let link = Link::connect(host, &directory, &relays, capabilities)?;
    println!("connected to {host} in {:?}", started.elapsed());

    let counter = Arc::new(Counter::default());
    link.present_on(Arc::clone(&counter) as Arc<dyn FrameSink>);

    std::thread::sleep(Duration::from_secs(seconds));

    let stats = link.stats();
    let decoded = counter.frames.load(Ordering::Relaxed);
    let received = counter.bytes.load(Ordering::Relaxed);
    let elapsed = counter
        .first
        .lock()
        .expect("counter mutex")
        .map_or(Duration::ZERO, |first| first.elapsed());

    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    println!(
        "delivered {decoded} frames, {received} bytes in {elapsed:?} => {:.1} fps, {:.2} Mbit/s",
        rate(decoded, seconds),
        rate(received * 8, seconds) / 1e6
    );
    println!("{stats:#?}");
    Ok(())
}

fn rate(total: u64, seconds: f64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frame or byte count in one probe is far below the exact integer range of a double"
    )]
    let total = total as f64;
    total / seconds
}
