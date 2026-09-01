//! Connects to a running host, watches the stream for a few seconds, and reports
//! what arrived.
//!
//! Run the daemon first, then `cargo run -p tachyx-driver --example connect --
//! 127.0.0.1:47989`.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tachyx_driver::{FrameSink, Link};
use tachyx_proto::control::{Capabilities, Size};

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
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:47989".to_owned());
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(6);

    let state = std::env::temp_dir().join("tachyx-connect-probe");
    std::fs::create_dir_all(&state)?;

    let capabilities = Capabilities {
        viewport: Size::new(2400, 1080).expect("a viewport"),
        max_decode: Size::new(3840, 2160).expect("a decode ceiling"),
        max_decode_fps: NonZeroU16::new(120).expect("a frame rate"),
        requested_fps: NonZeroU16::new(60).expect("a frame rate"),
        bitrate_ceiling: NonZeroU32::new(20_000_000).expect("a bitrate"),
    };

    let started = Instant::now();
    let link = Link::connect(&address, &state, capabilities)?;
    println!("connected to {address} in {:?}", started.elapsed());

    let counter = Arc::new(Counter::default());
    link.present_on(Arc::clone(&counter) as Arc<dyn FrameSink>);

    std::thread::sleep(Duration::from_secs(seconds));

    let stats = link.stats();
    let frames = counter.frames.load(Ordering::Relaxed);
    let bytes = counter.bytes.load(Ordering::Relaxed);
    let elapsed = counter
        .first
        .lock()
        .expect("counter mutex")
        .map_or(Duration::ZERO, |first| first.elapsed());

    println!(
        "decoded {frames} frames, {bytes} bytes in {elapsed:?} => {:.1} fps, {:.2} Mbit/s",
        frames as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
        (bytes as f64 * 8.0) / elapsed.as_secs_f64().max(f64::EPSILON) / 1e6
    );
    println!("{stats:#?}");
    Ok(())
}
