//! Captures an output, encodes it through `vapostproc` and one of the VA
//! encoders, and writes the result as an elementary stream.
//!
//! The point of this example is the shared buffer. Everything else here is
//! reporting: if the descriptor the compositor filled does not import into the
//! scaler, nothing downstream of it happens at all.
//!
//! Two modes. Without `--live` it encodes one settling of the pipeline and
//! reports its latency, which is the only way to get percentiles that belong to
//! one size, because the encoder's sample window spans everything it has done.
//! With `--live` it walks the resolution ladder, the bitrate and the declared
//! frame rate on a pipeline that is never torn down, and reports what each
//! change cost.
//!
//! ```zsh
//! nix develop --command cargo run --release -p tachyx-media \
//!   --example encode -- --output DP-2 --dir /tmp/enc --frames 400 --codec av1
//! ```

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use tachyx_media::{
    AccessUnits, Bitrate, Capture, CaptureStats, EncodeConfig, EncodeSize, EncodeStats, Encoder,
    Framerate, Rate,
};
use tachyx_proto::control::Codec;
use tachyx_proto::media::FrameKind;

/// The codec to ask the host's hardware for, as a command line word.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Wanted {
    /// H.264.
    H264,
    /// AV1.
    Av1,
}

impl Wanted {
    /// What the bitstream files are named, so a player picks the right parser.
    fn suffix(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Av1 => "obu",
        }
    }
}

impl From<Wanted> for Codec {
    fn from(wanted: Wanted) -> Self {
        match wanted {
            Wanted::H264 => Self::H264,
            Wanted::Av1 => Self::Av1,
        }
    }
}

/// The codec asked for, checked against what this machine really has.
fn chosen(wanted: Wanted) -> Result<Codec, Box<dyn std::error::Error>> {
    let codec = Codec::from(wanted);
    let available = tachyx_media::encoders()?;
    println!("encoders    {available:?}");
    if available.contains(codec) {
        Ok(codec)
    } else {
        Err(format!("this machine has no {codec} encoder").into())
    }
}

/// What to capture and encode, and where to put it.
#[derive(Debug, Parser)]
struct Args {
    /// Output to capture, as the compositor names it.
    #[arg(long, default_value = "DP-2")]
    output: String,
    /// Codec to encode in. It must be one this machine really has.
    #[arg(long, value_enum, default_value_t = Wanted::H264)]
    codec: Wanted,
    /// Access units to collect in each phase.
    #[arg(long, default_value_t = 400)]
    frames: usize,
    /// Encoded height. Zero keeps the capture's own size, which is the rung
    /// that makes the scaler pass through.
    #[arg(long, default_value_t = 0)]
    height: u32,
    /// Target bitrate in kilobits per second.
    #[arg(long, default_value_t = 20_000)]
    kbps: u32,
    /// Directory to write the elementary streams into.
    #[arg(long)]
    dir: PathBuf,
    /// Walk the ladder, the bitrate and the frame rate on one running pipeline.
    #[arg(long)]
    live: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    std::fs::create_dir_all(&args.dir)?;

    let codec = chosen(args.codec)?;

    let (capture, frames) = Capture::start(&args.output)?;
    let format = capture.format().clone();
    let native = EncodeSize::new(format.size.width(), format.size.height())?;
    let size = if args.height == 0 {
        native
    } else {
        EncodeSize::fit(format.size, args.height)?
    };
    let rate = Rate {
        bitrate: Bitrate::from_kbps(args.kbps).ok_or("a bitrate of zero")?,
        framerate: format.refresh,
    };
    println!("capture     {} at {}", format.size, format.refresh);
    println!(
        "layout      {} modifier {:#018x} on {}",
        fourcc(format.fourcc),
        format.modifier,
        format.render_node.display()
    );
    println!("encode      {codec} {} at {}", size.visible(), rate.bitrate);

    let (encoder, units) = Encoder::start(frames, EncodeConfig { codec, size, rate })?;
    let geometry = encoder.geometry();
    println!(
        "geometry    coded {} visible {} aspect {}:{}",
        geometry.coded, geometry.visible, geometry.display_aspect.0, geometry.display_aspect.1
    );

    let mut run = Run {
        capture,
        encoder,
        units,
        dir: args.dir.clone(),
        suffix: args.codec.suffix(),
        frames: args.frames,
        seen: Marks::default(),
    };
    run.settle()?;
    println!("\n{}", heading());

    if args.live {
        run.phase("native", |_| Ok(()))?;
        for height in [1080, 720] {
            let rung = EncodeSize::fit(format.size, height)?;
            run.phase(&format!("{}", rung.visible()), |run| {
                run.encoder.set_resolution(rung);
                Ok(())
            })?;
        }
        run.phase("back to native", |run| {
            run.encoder.set_resolution(native);
            Ok(())
        })?;
        let doubled = Bitrate::from_kbps(args.kbps * 2).ok_or("a bitrate of zero")?;
        run.phase("bitrate doubled", |run| {
            Ok(run.encoder.set_rate(Rate {
                bitrate: doubled,
                framerate: rate.framerate,
            })?)
        })?;
        let halved =
            Framerate::from_millihertz(rate.framerate.millihertz() / 2).ok_or("no frame rate")?;
        run.phase("framerate halved", |run| {
            Ok(run.encoder.set_rate(Rate {
                bitrate: doubled,
                framerate: halved,
            })?)
        })?;
        run.gate()?;
    } else {
        run.phase("steady", |_| Ok(()))?;
    }

    let stats = run.encoder.stats();
    println!(
        "\nencoder latency p50 {:.2} ms  p90 {:.2} ms  p99 {:.2} ms  max {:.2} ms  over {} units",
        millis(stats.latency.p50),
        millis(stats.latency.p90),
        millis(stats.latency.p99),
        millis(stats.latency.max),
        stats.encoded
    );
    println!(
        "filler {:.3}% of {} bytes, refused {}",
        stats.filler_fraction() * 100.0,
        stats.bytes + stats.filler_bytes,
        stats.refused
    );
    let ring = run.capture.stats();
    println!(
        "capture captured {} dropped {} failed {} starved {}",
        ring.captured, ring.dropped, ring.failed, ring.starved
    );
    run.encoder.stop()?;
    run.capture.stop()?;
    Ok(())
}

/// Everything one run needs to keep, so a phase is a closure and a count.
struct Run {
    capture: Capture,
    encoder: Encoder,
    units: AccessUnits,
    dir: PathBuf,
    /// See [`Wanted::suffix`].
    suffix: &'static str,
    frames: usize,
    seen: Marks,
}

/// The counters as they stood at the end of the last phase.
#[derive(Debug, Default, Clone, Copy)]
struct Marks {
    encode: EncodeStats,
    capture: CaptureStats,
}

impl Run {
    /// Encodes a few frames and throws them away, so the first phase measures
    /// the pipeline rather than its first negotiation.
    fn settle(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..30 {
            self.units.recv().ok_or_else(|| self.stopped())?;
        }
        self.mark();
        Ok(())
    }

    fn mark(&mut self) {
        self.seen = Marks {
            encode: self.encoder.stats(),
            capture: self.capture.stats(),
        };
    }

    /// Applies a change, then collects a phase's worth of access units and
    /// reports what the change cost.
    fn phase(
        &mut self,
        label: &str,
        change: impl FnOnce(&Self) -> Result<(), Box<dyn std::error::Error>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = self
            .dir
            .join(format!("{}.{}", label.replace(' ', "-"), self.suffix));
        let mut file = File::create(&path)?;
        // The file has to open on a key frame to decode on its own. A phase
        // that started wherever the last one left off would reference the
        // parameter sets of a key frame far behind it, which is exactly what a
        // client joining late would see.
        let prologue = self.until_key(&mut file)?;

        let applying = Instant::now();
        change(self)?;
        let applied = applying.elapsed();

        let started = Instant::now();
        let mut previous = started;
        let mut first_gap = Duration::ZERO;
        let mut worst_gap = Duration::ZERO;
        let mut first_key = None;
        for index in 0..self.frames {
            let unit = self.units.recv().ok_or_else(|| self.stopped())?;
            let now = Instant::now();
            let gap = now.saturating_duration_since(previous);
            previous = now;
            if index == 0 {
                first_gap = gap;
            } else {
                worst_gap = worst_gap.max(gap);
            }
            if first_key.is_none() && matches!(unit.kind, FrameKind::Key) {
                first_key = Some(index + 1);
            }
            file.write_all(&unit.bytes)?;
        }
        let elapsed = started.elapsed();
        file.flush()?;

        let before = self.seen;
        self.mark();
        let encode = self.seen.encode;
        let capture = self.seen.capture;
        let bytes = encode.bytes - before.encode.bytes;
        let filler = encode.filler_bytes - before.encode.filler_bytes;
        println!(
            "{label:<17}{:>5} {:>8.3} {:>7.1} {:>8.2} {:>9.2} {:>8.3} {:>7} {:>6} {:>8} {:>8} {:>6}",
            self.frames,
            elapsed.as_secs_f64(),
            per_second(self.frames, elapsed),
            millis(first_gap),
            millis(worst_gap),
            millis(applied),
            first_key.map_or("none".to_owned(), |at| at.to_string()),
            capture.dropped - before.capture.dropped,
            capture.starved - before.capture.starved,
            format!("{:.2}", megabits(bytes + filler, elapsed)),
            prologue,
        );
        Ok(())
    }

    /// Collects access units until a key frame has been written, so what
    /// follows can be decoded without anything before it.
    ///
    /// Returns how many units it took, which also says how long a client asking
    /// for recovery would wait.
    fn until_key(&mut self, file: &mut File) -> Result<usize, Box<dyn std::error::Error>> {
        self.encoder.force_keyframe();
        for taken in 1..=self.frames {
            let unit = self.units.recv().ok_or_else(|| self.stopped())?;
            file.write_all(&unit.bytes)?;
            if matches!(unit.kind, FrameKind::Key) {
                return Ok(taken);
            }
        }
        Err("the encoder never produced a key frame when asked".into())
    }

    /// Closes the admission gate for a second, then opens it again.
    ///
    /// A refused frame must never reach the encoder, so the counter that moves
    /// is the encoder's own refusal count while capture carries on untouched.
    fn gate(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let admission = self.encoder.admission();
        let before = self.seen;
        admission.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        let shut = self.encoder.stats();
        let ring = self.capture.stats();
        admission.store(true, Ordering::Relaxed);
        println!(
            "\ngate shut for 1 s: refused {}, encoded {}, captured {}",
            shut.refused - before.encode.refused,
            shut.encoded - before.encode.encoded,
            ring.captured - before.capture.captured
        );
        self.mark();
        self.phase("gate reopened", |_| Ok(()))
    }

    /// Why there is no next access unit.
    ///
    /// The encoder reports what ended it through `tracing` as it is dropped,
    /// so this only has to say how far it got.
    fn stopped(&self) -> String {
        format!(
            "the encoder stopped answering after {} access units",
            self.encoder.stats().encoded
        )
    }
}

/// One line of column headings for [`Run::phase`].
fn heading() -> String {
    format!(
        "{:<17}{:>5} {:>8} {:>7} {:>8} {:>9} {:>8} {:>7} {:>6} {:>8} {:>8} {:>6}",
        "phase",
        "units",
        "seconds",
        "fps",
        "first ms",
        "worst ms",
        "apply ms",
        "key at",
        "drop",
        "starve",
        "Mbit/s",
        "forced"
    )
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a unit count that loses a bit at 2^53 is not a rate anybody reads"
)]
fn per_second(count: usize, elapsed: Duration) -> f64 {
    count as f64 / elapsed.as_secs_f64()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a byte count that loses a bit at 2^53 is not a bitrate anybody reads"
)]
fn megabits(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 * 8.0 / 1_000_000.0 / elapsed.as_secs_f64()
}

fn fourcc(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes())
        .trim_end()
        .to_owned()
}
