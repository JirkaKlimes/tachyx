//! Asking the login session for a scheduling priority on a thread that must not
//! wait.
//!
//! The threads that carry a live picture are not compute threads. Each one
//! sleeps most of the time and has to run the moment it is woken: capture wakes
//! on the compositor's frame, the encoder wakes on capture, the release loop
//! wakes on a schedule the pacer set to the microsecond. A wake-up served late
//! is a frame delivered late, and nothing further down the pipeline recovers
//! it.
//!
//! Measured in `spec/findings.md`, the send path alone costs a 99th percentile
//! of 2.5 ms on an idle machine and 7.9 ms on one that is compiling, with a
//! worst case of 10.5 ms against 2.8. The round trip moves with it, which is
//! what says the difference is the scheduler rather than the network. A remote
//! desktop whose host is doing nothing else is not the case worth designing
//! for.
//!
//! # Why `RealtimeKit`, and why the realtime class rather than a nice level
//!
//! There is no other unprivileged way to do this on Linux. `setpriority` below
//! nought needs `RLIMIT_NICE` and `sched_setscheduler` needs `RLIMIT_RTPRIO`,
//! and both are zero for an ordinary login: measured on the target machine,
//! `ulimit -e` and `ulimit -r` are 0 soft and 0 hard. A `setpriority` fallback
//! would be a call that always fails, which is worse than none because it reads
//! as cover. `RealtimeKit` is the broker that exists for exactly this, it runs on
//! the system bus of every desktop that runs PipeWire, and it is therefore
//! already present wherever this host can capture sound at all.
//!
//! The choice between its two offers was measured rather than assumed, by
//! `examples/wake.rs`, against 64 spinning processes on a 16-thread machine,
//! each in a session of its own. How late a one-millisecond sleep woke, over
//! 20,000 wake-ups:
//!
//! | | p50 | p90 | p99 | worst |
//! |---|---|---|---|---|
//! | ordinary | 0.006 ms | 0.016 ms | 2.977 ms | 46.409 ms |
//! | nice -11 | 0.007 ms | 0.017 ms | 2.987 ms | 30.115 ms |
//! | `SCHED_RR` 5 | 0.006 ms | 0.015 ms | **0.018 ms** | **0.047 ms** |
//!
//! **A nice level buys nothing above the ninetieth percentile.** The reason is
//! `kernel.sched_autogroup_enabled`, which is 1 on this machine and on every
//! desktop distribution: the fair scheduler shares the processor between
//! session groups first and between the threads inside a group second, so a
//! thread's nice level orders it against its own siblings and says nothing at
//! all about the compile in the next session. The realtime class sits above the
//! whole of that and is the only lever that reaches the case the tail comes
//! from.
//!
//! This is also what PipeWire asks the same broker for, on the same argument.
//!
//! # Why a realtime thread here cannot take the machine down
//!
//! Three separate bounds, none of them ours:
//!
//! - `kernel.sched_rt_runtime_us` is 950,000 of 1,000,000 on this machine, so
//!   the kernel reserves five per cent of every processor for ordinary threads
//!   however many realtime ones are runnable.
//! - `RLIMIT_RTTIME`, which [`raise`] sets to the broker's own maximum before it
//!   asks, is the kernel's dead-man switch: a realtime thread that holds a
//!   processor for that long without blocking is signalled and then killed. The
//!   broker refuses to promote a thread at all until the limit is set, which is
//!   the whole reason it insists.
//! - [`PRIORITY`] is low enough that the sound server and the kernel's own
//!   interrupt threads still outrank us.
//!
//! Every thread this is called on blocks on something within a frame: the
//! compositor, the encoder, the pacer's sleep or a socket. None of them spins.

/// The realtime priority the media threads ask for.
///
/// Deliberately near the bottom of the class rather than at the ceiling the
/// broker allows, which is 20 on this machine. Priority orders realtime threads
/// against each other and against nothing else, and the only other realtime
/// threads on a desktop are the sound server's and the kernel's interrupt
/// threads. Both should win: a glitch in the sound is worse than a frame that
/// is a millisecond late, and an interrupt thread that cannot run is a worse
/// problem than either.
#[cfg(target_os = "linux")]
const PRIORITY: u32 = 5;

/// The nice level to fall back to where the broker allows one but no realtime.
///
/// Measured worth little, and taken anyway because it costs one call: an
/// administrator who has set `MaxRealtimePriority` to zero and left
/// `MinNiceLevel` alone has said what they want, and the ninetieth percentile
/// does improve.
#[cfg(target_os = "linux")]
const NICE: i32 = -11;

/// Raises the calling thread's priority, and says once if it cannot.
///
/// Call it from inside the thread, first thing. `role` names the thread in the
/// log, so an operator reading "the encode thread runs at the ordinary
/// priority" knows which one and why the tail is what it is.
///
/// Never fails. A thread that keeps the priority it was given still does its
/// job, and there is nothing a caller could usefully do about a broker that is
/// not installed. A daemon that will not start without `CAP_SYS_NICE` is a
/// daemon nobody installs.
#[cfg(target_os = "linux")]
pub fn raise(role: &str) {
    let Some(broker) = broker() else { return };
    let thread = rtkit_rs::RTKit::current_thread_id();
    if let Err(realtime) = broker.make_thread_realtime(thread, PRIORITY) {
        // Both refusals are reported together or the message names a symptom
        // rather than a cause: an administrator who capped the realtime
        // priority and an absent `RLIMIT_RTTIME` look identical from the second
        // line alone.
        if let Err(nice) = broker.make_thread_high_priority(thread, NICE) {
            tracing::info!(
                %realtime,
                %nice,
                role,
                "the realtime broker refused: this thread runs at the ordinary priority"
            );
        } else {
            tracing::debug!(%realtime, role, nice = NICE, "raised a thread to a nice level");
        }
        return;
    }
    tracing::debug!(role, priority = PRIORITY, "raised a thread to realtime");
}

/// The broker, opened once for the process, with the dead-man switch armed.
///
/// One connection rather than one a thread: opening a system bus connection is
/// a socket, an authentication exchange and a name request, and a session
/// raises four threads. `RLIMIT_RTTIME` is per process anyway.
#[cfg(target_os = "linux")]
fn broker() -> Option<&'static rtkit_rs::RTKit> {
    static BROKER: std::sync::OnceLock<Option<rtkit_rs::RTKit>> = std::sync::OnceLock::new();
    BROKER
        .get_or_init(|| {
            let broker = match rtkit_rs::RTKit::new() {
                Ok(broker) => broker,
                Err(error) => {
                    tracing::info!(
                        %error,
                        "no realtime broker: the media threads run at the ordinary priority"
                    );
                    return None;
                }
            };
            // Asked for rather than chosen. The broker refuses a thread whose
            // process has a wider limit than its own, so the only value that
            // works is the one it names, and it is the kernel's bound on how
            // long one of these threads may hold a processor without blocking.
            let micros = broker.rttime_usec_max().ok().and_then(|max| {
                let max = u64::try_from(max).ok()?;
                rustix::process::setrlimit(
                    rustix::process::Resource::Rttime,
                    rustix::process::Rlimit {
                        current: Some(max),
                        maximum: Some(max),
                    },
                )
                .ok()?;
                Some(max)
            });
            if let Some(micros) = micros {
                tracing::debug!(micros, "armed the realtime time limit");
            } else {
                // The broker is kept anyway: the nice level in `raise` needs no
                // limit, and it is what is left.
                tracing::info!("the realtime time limit could not be set");
            }
            Some(broker)
        })
        .as_ref()
}

/// Raises the calling thread's priority, where the platform has a way to.
///
/// This one has none that is reachable from here. Android schedules a media
/// thread through `PerformanceHintManager`, which is a Java interface and
/// belongs beside the decoder that knows the deadline.
#[cfg(not(target_os = "linux"))]
pub fn raise(role: &str) {
    let _ = role;
}
