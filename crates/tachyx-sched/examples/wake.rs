//! What the priority buys, measured on the machine it is asked for on.
//!
//! The quantity is the one thing every hot thread in this project does: sleep
//! for a short, exact time and then run. The transport's release loop sleeps to
//! the pacer's schedule, capture sleeps on the compositor, the encoder sleeps
//! on capture. What this measures is how late the wake-up is, which is the
//! whole of what a priority changes.
//!
//! It has to be run against a loaded machine, because an idle one has nothing
//! to measure: a scheduler with a free processor serves a waking thread at once
//! whatever its priority is.
//!
//! **The load must be in sessions of its own.** With `sched_autogroup_enabled`,
//! which is the default everywhere, the fair scheduler shares the processor
//! between session groups before it shares it between the threads inside one.
//! Load spun up inside this process is load this process is already paying for,
//! and it makes a nice level look like it works. The real case is a compile in
//! another terminal. Pass a thread count to load in-process anyway, to see the
//! difference for yourself.
//!
//! ```zsh
//! for i in $(seq 1 64); do setsid sh -c 'while :; do :; done' & done
//! nix develop --command cargo run --release -p tachyx-sched --example wake
//! kill %1  # and the rest
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What the release loop sleeps for, and so what this sleeps for.
const PERIOD: Duration = Duration::from_millis(1);

/// Sleeps to measure. Twenty seconds at a millisecond each, which is what the
/// tail needs: the same run over two seconds moved by a factor of four between
/// repeats.
const WAKES: usize = 20_000;

/// What a thread was asked to run as.
#[derive(Debug, Clone, Copy)]
enum As {
    /// Whatever the process was started with.
    Ordinary,
    /// Whatever [`tachyx_sched::raise`] can get, which is what ships.
    Raised,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let threads: usize = std::env::args()
        .nth(1)
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if threads == 0 {
        println!("measuring against whatever else is running");
    } else {
        println!("loading {threads} threads inside this process, which is the weaker test");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = (0..threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut spun = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    spun = spun.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                }
                spun
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));

    // Ordinary first, so that the raised run cannot be the one that also
    // benefits from a machine the other warmed.
    let ordinary = measure(As::Ordinary);
    let raised = measure(As::Raised);

    stop.store(true, Ordering::Relaxed);
    for thread in load {
        let _ = thread.join();
    }

    println!(
        "\n  wake-up late     p50        p90        p99        worst\n  \
           ordinary      {:>7.3} ms {:>7.3} ms {:>7.3} ms {:>7.3} ms\n  \
           raised        {:>7.3} ms {:>7.3} ms {:>7.3} ms {:>7.3} ms\n",
        millis(ordinary[0]),
        millis(ordinary[1]),
        millis(ordinary[2]),
        millis(ordinary[3]),
        millis(raised[0]),
        millis(raised[1]),
        millis(raised[2]),
        millis(raised[3]),
    );
}

/// One run of [`WAKES`] sleeps, on a thread of its own so that the priority
/// applies to that thread and to nothing else.
///
/// Returns the 50th, 90th and 99th percentile and the worst of how late each
/// wake-up was.
fn measure(run: As) -> [Duration; 4] {
    std::thread::spawn(move || {
        if matches!(run, As::Raised) {
            tachyx_sched::raise("the measurement");
        }
        let mut late = Vec::with_capacity(WAKES);
        for _ in 0..WAKES {
            let due = Instant::now() + PERIOD;
            std::thread::sleep(PERIOD);
            late.push(Instant::now().saturating_duration_since(due));
        }
        late.sort_unstable();
        let at = |share: usize| late[(late.len() * share / 100).min(late.len() - 1)];
        [at(50), at(90), at(99), late[late.len() - 1]]
    })
    .join()
    .expect("the measurement thread does not panic")
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
