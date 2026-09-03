//! The sound half of one session.
//!
//! Three things meet here and nowhere else: the capture and encode pipeline in
//! `tachyx-media`, the connection to the sound server in [`crate::audio`], and
//! the sender in `tachyx-transport` that puts packets on the wire past the
//! pacer.
//!
//! The mode is what the host is **really** doing, not what it was asked for. A
//! pipeline that will not start is reported as off, and the client shows off,
//! rather than showing a control that says sound is playing while nothing is.
//!
//! Exclusive mode mutes the host and the tap is before the mute, so the client
//! hears everything while the room hears nothing. What that buys is also what
//! makes it dangerous: the machine is left silent if nothing puts the mute
//! back. So the mute is held by a guard rather than by a pair of calls, and
//! every way a session can end runs the same restore, disconnection and panic
//! included.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tachyx_media::{AUDIO_BITRATE, AudioCapture, Clock, OpusPackets};
use tachyx_proto::control::{AudioCommand, AudioMode, AudioStatus};
use tachyx_proto::media::FrameId;
use tachyx_transport::{AudioError, AudioSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::audio::{HostAudio, Silenced, Sink};

/// Bits a second sound takes off the link while it flows.
///
/// The encoder's own rate plus the media header every packet carries. A hundred
/// packets a second of sixteen bytes each is 12.8 kbit/s, a tenth of the sound
/// itself, which is too much to leave out of the arithmetic.
///
/// It comes off the video target rather than going on top of it. Sound is
/// neither paced nor gated: it leaves the moment it is encoded. Sent on top,
/// the link carries the sum of the two, and every packet of the excess is one
/// the estimator reads as the picture asking for too much.
const AUDIO_RESERVE: u32 = AUDIO_BITRATE + 12_800;

/// The sound half of a session, from the sink it records to the wire.
#[derive(Debug)]
pub struct Sound {
    control: HostAudio,
    sender: AudioSender,
    /// The session's epoch, which is the picture's epoch. Two streams stamped
    /// from two starts cannot be laid against each other.
    clock: Clock,
    /// What the host is really doing.
    mode: AudioMode,
    capture: Option<AudioCapture>,
    pump: Option<JoinHandle<()>>,
    /// The next packet's number.
    ///
    /// It survives a stop and a restart, and it has to. A counter that went
    /// back to zero on the second start would look to the receiver like four
    /// thousand million packets lost.
    next: Arc<AtomicU32>,
    /// Held for as long as the host is meant to be silent.
    silence: Option<Silenced>,
}

impl Sound {
    /// Takes the pieces a session has and holds them together.
    ///
    /// Nothing is captured until a client asks for it: a session opens in
    /// [`AudioMode::Off`], which is the host untouched.
    #[must_use]
    pub fn new(control: HostAudio, sender: AudioSender, clock: Clock) -> Self {
        Self {
            control,
            sender,
            clock,
            mode: AudioMode::Off,
            capture: None,
            pump: None,
            next: Arc::new(AtomicU32::new(0)),
            silence: None,
        }
    }

    /// What the host is doing and what its output is set to.
    #[must_use]
    pub fn status(&self) -> AudioStatus {
        let sink = self.control.sink();
        AudioStatus {
            mode: self.mode,
            volume: sink.volume,
            muted: sink.muted,
        }
    }

    /// A view of the host's own output that wakes whenever it changes.
    ///
    /// A knob turned on the host reaches the client through this. Nothing
    /// polls, and nothing has to ask.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<Sink> {
        self.control.changes()
    }

    /// Bits a second to keep back from the picture, which is none while no
    /// sound is flowing.
    #[must_use]
    pub const fn reserve(&self) -> u32 {
        match self.mode {
            AudioMode::Off => 0,
            AudioMode::Exclusive | AudioMode::Shared => AUDIO_RESERVE,
        }
    }

    /// Carries out one command, and reports what the host is doing afterwards.
    ///
    /// The answer is always the truth rather than an echo of the request. A
    /// volume the server clamps comes back clamped, and a mode that could not
    /// be started comes back off.
    pub async fn apply(&mut self, command: AudioCommand) -> AudioStatus {
        match command {
            AudioCommand::SetMode(wanted) => self.set_mode(wanted).await,
            AudioCommand::SetVolume(permille) => self.control.set_volume(permille),
            AudioCommand::SetMuted(muted) => self.control.set_muted(muted),
        }
        self.status()
    }

    /// Starts or stops capture, and decides the host's own silence with it.
    ///
    /// Only the crossing to and from [`AudioMode::Off`] touches the pipeline.
    /// Exclusive and shared differ in the host's own silence and in nothing
    /// else, so cycling between them must not restart capture: that would be a
    /// tenth of a second of nothing to hear, spent on a control a user presses
    /// to move one step.
    async fn set_mode(&mut self, wanted: AudioMode) {
        tracing::info!(?wanted, current = ?self.mode, "sound: mode change requested");
        if wanted == self.mode {
            return;
        }
        if matches!(wanted, AudioMode::Off) {
            self.stop();
            return;
        }
        if self.capture.is_none() && !self.start().await {
            // The mode is what the host is really doing, so a pipeline that
            // would not start leaves it where it was.
            return;
        }
        // Assigning the guard is what applies it, and assigning `None` is what
        // puts the host's own mute back.
        self.silence = match wanted {
            AudioMode::Exclusive => Some(self.control.silence()),
            AudioMode::Off | AudioMode::Shared => None,
        };
        self.mode = wanted;
    }

    /// Ends capture and gives the host its own sound back.
    fn stop(&mut self) {
        self.mode = AudioMode::Off;
        // First, and on its own line. The host must not stay silent for a
        // moment longer than the mode that asked for it.
        drop(self.silence.take());
        drop(self.pump.take());
        if let Some(capture) = self.capture.take() {
            // Dropping it joins a GStreamer thread, which is not work for a
            // runtime worker: every other task that worker holds would wait
            // with it.
            drop(tokio::task::spawn_blocking(move || drop(capture)));
        }
    }

    /// Brings up the pipeline and the task that drains it.
    ///
    /// `false` when it would not start, which the caller reports as
    /// [`AudioMode::Off`] because that is what the host is then doing.
    async fn start(&mut self) -> bool {
        // Asked for now rather than remembered from the start of the session:
        // the default sink moves when somebody plugs in headphones, and the
        // monitor to record moves with it.
        let device = self.control.sink().monitor();
        tracing::info!(%device, "sound: starting capture on monitor");
        let clock = self.clock;
        // Off a runtime worker: it waits for the sound server to answer and for
        // the pipeline to reach playing.
        let started = tokio::task::spawn_blocking(move || AudioCapture::start(&device, clock)).await;
        match started {
            Ok(Ok((capture, packets))) => {
                self.pump = Some(pump(
                    packets,
                    self.sender.clone(),
                    Arc::clone(&self.next),
                ));
                self.capture = Some(capture);
                tracing::info!("sound: capture started and pump spawned");
                true
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot capture this host's own sound");
                false
            }
            Err(error) => {
                tracing::warn!(%error, "the sound pipeline did not start");
                false
            }
        }
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        // The mute is put back before the connection that puts it back is
        // closed. Declaring the fields in the right order would do it too, and
        // would be a rule that is invisible at the point it matters.
        drop(self.silence.take());
    }
}

/// Puts encoded sound on the wire, on a thread of its own.
///
/// A thread because the wait for the next packet is a blocking read, and a
/// thread of its own because the picture's pump spends most of its life asleep
/// between paced datagrams and sound cannot wait behind it.
///
/// Nothing here is paced and nothing is gated. The only thing that ends it is
/// capture stopping, or the connection going away.
fn pump(packets: OpusPackets, sender: AudioSender, next: Arc<AtomicU32>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while let Some(packet) = packets.recv() {
            // Spent before the send and whatever the send does. The number is
            // what the receiver reads loss from, so a packet that does not
            // reach the wire has to leave its own hole: the receiver conceals
            // the sound that was in it rather than playing the next packet as
            // though it followed the last.
            let id = FrameId(next.fetch_add(1, Ordering::Relaxed));
            match sender.send(id, packet.captured, &packet.bytes) {
                Ok(()) => {}
                // This packet's problem and not the session's. A queue with no
                // room drains, a path that shrank is being corrected, and an
                // encoder in discontinuous transmission is saying there was
                // silence to send.
                Err(
                    error @ (AudioError::NoRoom | AudioError::TooLarge | AudioError::Empty),
                ) => {
                    tracing::debug!(%error, "a packet of sound was dropped");
                }
                Err(error) => {
                    tracing::warn!(%error, "sound path ended");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use tachyx_proto::media::HEADER_LEN;

    use tachyx_media::AUDIO_FRAME;

    use super::{AUDIO_BITRATE, AUDIO_RESERVE};

    /// The reserve is written as one number so that it can be read, and checked
    /// here against the two figures it comes from.
    #[test]
    fn the_reserve_is_the_encoder_plus_the_header_on_every_packet() {
        let per_second = 1000 / u32::try_from(AUDIO_FRAME.as_millis()).expect("a short frame");
        assert_eq!(per_second, 100);
        let headers = per_second * u32::try_from(HEADER_LEN).expect("a short header") * 8;
        assert_eq!(headers, 12_800);
        assert_eq!(AUDIO_RESERVE, AUDIO_BITRATE + headers);
        // Well under half the target a session opens on, so reserving it
        // never collapses the picture on its own.
        const { assert!(AUDIO_RESERVE * 2 < tachyx_congestion::START_TARGET) }
    }
}
