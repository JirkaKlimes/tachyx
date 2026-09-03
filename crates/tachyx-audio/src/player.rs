//! The player: one thread, one sound device, and the state the two share.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;

use tachyx_client::AudioEvent;

use crate::decode::Voice;
use crate::device;
use crate::jitter::{Jitter, Next};
use crate::stats::Stats;

/// How many frames the player invents into an empty buffer before it stops
/// writing at all.
///
/// Twenty, which is two hundred milliseconds. Sound stops for two reasons and
/// they need opposite answers: a gap in a stream that is still playing has to be
/// covered, and a stream that has ended has to be left alone. This is the line
/// between them. Past it the thread parks until sound arrives again, which costs
/// a session with the sound turned off exactly nothing.
const QUIET_FRAMES: u32 = 20;

/// Why the play path could not be built or run.
///
/// Two cases, and neither is one a caller can do anything about beyond showing
/// it. That is why the platform's own failure is carried as its own words rather
/// than sorted into variants: a machine with no sound server and a machine whose
/// sound server refused the format are the same answer to the only question the
/// application asks, which is what to tell the user.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// libopus would not build a decoder.
    #[error("the Opus decoder would not start: {0}")]
    Decoder(#[source] opus::Error),
    /// This machine would not give the session a sound output.
    #[error("this machine has no sound output the client can use: {0}")]
    Output(String),
}

/// A stream of sound, played on this machine.
///
/// Build one, hand it what the session decided through [`AudioPlayer::push`],
/// and tell it how loud with [`AudioPlayer::set_gain`]. Everything else happens
/// on a thread of its own, paced by the sound device.
#[derive(Debug)]
pub struct AudioPlayer {
    shared: Arc<Shared>,
    /// Taken in [`Drop`], which is the only place it is `None`.
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Opens the machine's sound output and starts playing whatever is pushed.
    ///
    /// The device is opened on the thread that will use it, not here, because on
    /// Android an AAudio stream is a handle that belongs to one thread. This
    /// waits for that thread to say whether it worked, so a caller still learns
    /// of a failure at the call that caused it.
    ///
    /// # Errors
    /// If libopus will not build a decoder, or this machine will not give out an
    /// output at 48 kHz in stereo.
    pub fn new() -> Result<Self, Error> {
        let shared = Arc::new(Shared::new());
        let (ready, opened) = std::sync::mpsc::channel();
        let playing = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tachyx-audio".to_owned())
            .spawn(move || play(&playing, &ready))
            .map_err(|error| Error::Output(error.to_string()))?;
        match opened.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                thread: Some(thread),
            }),
            Ok(Err(refused)) => {
                let _ = thread.join();
                Err(refused)
            }
            // The thread cannot end without answering, so this is the thread
            // having panicked before it opened anything.
            Err(_) => Err(Error::Output(
                "the play thread stopped before it opened an output".to_owned(),
            )),
        }
    }

    /// Takes one thing the session decided, in the order it was captured.
    ///
    /// Called from the client's receive loop, and never blocks for longer than
    /// the play thread holds the buffer, which is the length of one push or one
    /// take.
    pub fn push(&self, event: AudioEvent) {
        self.shared.locked().push(event);
        self.shared.woken.notify_one();
    }

    /// Sets how loud what follows is played, as a factor of the samples the host
    /// sent.
    ///
    /// The host's volume is what this comes from: the capture tap sits ahead of
    /// the host's own volume control, so the stream carries full scale sound
    /// whatever the host's sink is set to, and this is where the setting is
    /// applied. It reaches the sound over one frame rather than at once. Values
    /// outside nought to one are bounded, because a gain over one clips.
    pub fn set_gain(&self, gain: f32) {
        self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// What the path has done so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.shared.locked().stats()
    }

    /// The first thing that went wrong, if anything has.
    ///
    /// A device that fails mid-session stops the thread, and without somewhere
    /// to read the reason a session that went silent gives none.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.shared.failure.get().cloned()
    }

    /// Stops playing and lets the device go.
    ///
    /// Called by [`Drop`] as well, so a caller that only drops the player loses
    /// nothing by it.
    pub fn finish(&self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        self.shared.woken.notify_all();
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.finish();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Everything the two threads touch.
#[derive(Debug)]
struct Shared {
    jitter: Mutex<Jitter>,
    woken: Condvar,
    /// The gain to reach, as the bits of an `f32`. Read once a frame by the play
    /// thread and written by whoever the volume control belongs to, so it is a
    /// number rather than a lock.
    gain: AtomicU32,
    stopping: AtomicBool,
    failure: OnceLock<String>,
}

impl Shared {
    /// Starts at full scale, which is the samples the host captured, untouched.
    ///
    /// The host reports its own volume and the client turns that into a gain
    /// below this one. Starting at silence instead would mean a host that never
    /// reported anything played nothing, and a host that is streaming has
    /// already proved it can be heard.
    fn new() -> Self {
        Self {
            jitter: Mutex::new(Jitter::default()),
            woken: Condvar::new(),
            gain: AtomicU32::new(1.0f32.to_bits()),
            stopping: AtomicBool::new(false),
            failure: OnceLock::new(),
        }
    }

    /// The buffer, whether or not a thread died holding it.
    ///
    /// A panicked lock here would mean the play thread stopped mid-frame, and
    /// refusing to play anything afterwards would be the larger fault.
    fn locked(&self) -> MutexGuard<'_, Jitter> {
        self.jitter.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Parks until there is sound to play. `false` means the player is done.
    fn wait_for_sound(&self) -> bool {
        let mut jitter = self.locked();
        while jitter.depth() == 0 {
            if self.stopping() {
                return false;
            }
            jitter = self
                .woken
                .wait(jitter)
                .unwrap_or_else(PoisonError::into_inner);
        }
        !self.stopping()
    }

    /// Records the first thing that went wrong, and only the first: what follows
    /// is almost always that failure repeating.
    fn fail(&self, failure: String) {
        tracing::error!(%failure, "the play path failed");
        let _ = self.failure.set(failure);
    }
}

/// What the play thread is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// Nothing is playing, and nothing is being written to the device.
    Idle,
    /// Sound has started arriving and is being gathered before it is played.
    Filling,
    /// Playing what is in the buffer.
    Playing,
}

/// The play thread: opens the device, then writes one frame at a time until the
/// player is dropped or the device stops taking them.
///
/// Every write blocks until the device has room, so this loop runs at the rate
/// the speaker consumes and needs no clock of its own.
fn play(shared: &Arc<Shared>, ready: &Sender<Result<(), Error>>) {
    let mut voice = match Voice::new() {
        Ok(voice) => voice,
        Err(error) => {
            let _ = ready.send(Err(Error::Decoder(error)));
            return;
        }
    };
    let mut speaker = match device::open() {
        Ok(speaker) => speaker,
        Err(refused) => {
            tracing::warn!(%refused, "sound: could not open the output device");
            let _ = ready.send(Err(Error::Output(refused)));
            return;
        }
    };
    tracing::info!("sound: output device open, play thread running");
    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut feed = Feed::Idle;
    let mut quiet = 0;
    while !shared.stopping() {
        let waiting = match feed {
            Feed::Idle => {
                if !shared.wait_for_sound() {
                    break;
                }
                // The sound before the silence is not what the sound after it
                // continues from.
                voice.restart();
                quiet = 0;
                feed = Feed::Filling;
                continue;
            }
            Feed::Filling if shared.locked().ready() => {
                feed = Feed::Playing;
                continue;
            }
            Feed::Filling => None,
            Feed::Playing => {
                let taken = shared.locked().take();
                if taken.is_none() {
                    // The device is still asking and the buffer has nothing. The
                    // depth it was playing on is gone, so it is gathered again
                    // rather than played from empty, which would underrun on
                    // every frame from here on.
                    shared.locked().starved();
                    feed = Feed::Filling;
                }
                taken
            }
        };
        let next = if let Some(next) = waiting {
            quiet = 0;
            next
        } else {
            shared.locked().invented();
            quiet += 1;
            Next::Conceal(None)
        };
        if let Err(refused) = speaker.write(voice.render(&next, shared.gain())) {
            shared.fail(refused);
            break;
        }
        if quiet >= QUIET_FRAMES {
            feed = Feed::Idle;
        }
    }
    speaker.finish();
}
