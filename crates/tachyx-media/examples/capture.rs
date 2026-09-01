//! Captures frames from one output and reports what the ring did.
//!
//! Two phases. The first pulls a fixed number of frames as fast as the
//! compositor supplies them, which under motion is the refresh rate. The second
//! sits on whatever the screen is doing by itself and reports both the frame
//! count and the process's own processor time, because the claim being tested is
//! that an idle screen costs nothing rather than that it costs a spin.
//!
//! ```zsh
//! nix develop --command cargo run --release -p tachyx-media \
//!   --example capture -- --output eDP-1 --frames 600
//! ```

use std::time::{Duration, Instant};

use clap::Parser;
use tachyx_media::Capture;

/// What to capture, and for how long.
#[derive(Debug, Parser)]
struct Args {
    /// Output to capture, as the compositor names it.
    #[arg(long, default_value = "eDP-1")]
    output: String,
    /// Frames to pull before reporting.
    #[arg(long, default_value_t = 600)]
    frames: usize,
    /// Seconds to spend on a static screen afterwards.
    #[arg(long, default_value_t = 5)]
    idle: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    let (capture, frames) = Capture::start(&args.output)?;
    let format = capture.format();
    println!("output      {}", format.output);
    println!("size        {}", format.size);
    println!("refresh     {}", format.refresh);
    println!(
        "layout      {} modifier {:#018x}",
        fourcc(format.fourcc),
        format.modifier
    );
    println!("node        {}", format.render_node.display());
    println!("planes      {:?}", format.planes);
    println!("length      {} bytes", format.length);

    let mut waits = Vec::with_capacity(args.frames);
    let mut ages = Vec::with_capacity(args.frames);
    let first = frames
        .next()
        .ok_or("capture ended before the first frame")?;
    drop(first);
    let started = Instant::now();
    for _ in 0..args.frames {
        let waiting = Instant::now();
        let Some(frame) = frames.next() else {
            break;
        };
        waits.push(waiting.elapsed());
        ages.push(frame.age());
    }
    let elapsed = started.elapsed();
    let motion = capture.stats();

    println!("\n-- motion --");
    println!("frames      {}", waits.len());
    println!("elapsed     {:.3} s", elapsed.as_secs_f64());
    println!("rate        {:.1} fps", rate(waits.len(), elapsed));
    report("wait", &mut waits);
    report("age", &mut ages);
    println!(
        "captured {} dropped {} failed {} starved {}",
        motion.captured, motion.dropped, motion.failed, motion.starved
    );

    let drain = std::thread::spawn(move || {
        let mut seen = 0u64;
        while frames.next().is_some() {
            seen += 1;
        }
        seen
    });
    let cpu = processor_time()?;
    let idle_for = Duration::from_secs(args.idle);
    std::thread::sleep(idle_for);
    let cpu = processor_time()?.saturating_sub(cpu);
    let idle = capture.stats();
    capture.stop()?;
    let seen = drain.join().map_err(|_| "the drain thread panicked")?;

    println!("\n-- idle --");
    println!("seconds     {}", args.idle);
    println!("frames      {seen}");
    println!("rate        {:.2} fps", rate_u64(seen, idle_for));
    println!(
        "processor   {:.1} ms, {:.2}% of one core",
        cpu.as_secs_f64() * 1000.0,
        100.0 * cpu.as_secs_f64() / idle_for.as_secs_f64()
    );
    println!(
        "captured {} dropped {} failed {} starved {}",
        idle.captured - motion.captured,
        idle.dropped - motion.dropped,
        idle.failed - motion.failed,
        idle.starved - motion.starved
    );
    Ok(())
}

/// Prints the median, the ninetieth percentile and the worst of a sample.
fn report(name: &str, samples: &mut [Duration]) {
    if samples.is_empty() {
        println!("{name:<12}(no samples)");
        return;
    }
    samples.sort_unstable();
    let at = |share: usize| samples[(samples.len() * share / 100).min(samples.len() - 1)];
    println!(
        "{name:<12}p50 {:.2} ms  p90 {:.2} ms  max {:.2} ms",
        millis(at(50)),
        millis(at(90)),
        millis(samples[samples.len() - 1])
    );
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a frame count that loses a bit at 2^53 is not a rate anybody reads"
)]
fn rate(frames: usize, elapsed: Duration) -> f64 {
    frames as f64 / elapsed.as_secs_f64()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a frame count that loses a bit at 2^53 is not a rate anybody reads"
)]
fn rate_u64(frames: u64, elapsed: Duration) -> f64 {
    frames as f64 / elapsed.as_secs_f64()
}

/// Processor time this process has spent, over every thread.
///
/// The unit `/proc` reports in is fixed at a hundredth of a second whatever the
/// kernel's own tick. The command name is taken off first because it is the one
/// field that may itself contain spaces and brackets.
fn processor_time() -> Result<Duration, Box<dyn std::error::Error>> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let (_, tail) = stat
        .rsplit_once(')')
        .ok_or("no fields after the command name")?;
    let mut fields = tail.split_whitespace().skip(11);
    let mut ticks = || -> u64 {
        fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    };
    Ok(Duration::from_millis((ticks() + ticks()) * 10))
}

fn fourcc(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes())
        .trim_end()
        .to_owned()
}
