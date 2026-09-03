//! The wire's audio types and what the play path counts, described so that Dart
//! gets the same ones.
//!
//! The mode below mirrors [`tachyx_proto::control::AudioMode`], so the generated
//! Dart enum is the protocol's own rather than a second one that has to be kept
//! in step. A change to the protocol breaks this file at compile time instead of
//! breaking the phone at run time.

use flutter_rust_bridge::frb;
pub use tachyx_proto::control::AudioMode;

/// Mirror of [`AudioMode`].
#[frb(mirror(AudioMode))]
#[derive(Debug)]
pub enum AudioModeMirror {
    /// Nothing is captured and nothing is streamed. The host's own output is
    /// left exactly as it was found.
    Off,
    /// Streamed to the client, with the host's own sink muted.
    Exclusive,
    /// Streamed to the client, and the host goes on playing it as well.
    Shared,
}

/// What the session is doing about sound.
///
/// Everything here is either the host's own word about its sink, which is
/// labelled as such, or something this client counted. The volume is the host's
/// and there is no second one: the capture tap sits ahead of the host's volume
/// control, so the client applies what the host reports and the two never
/// disagree.
#[derive(Debug, Clone, Copy)]
pub struct AudioStats {
    /// Whether the host said it can stream its own sound.
    ///
    /// The only source for that answer, and what decides whether the
    /// application offers a control at all. A control that is offered and then
    /// does nothing is worse than one that is not offered, because the user
    /// cannot tell it from a fault.
    pub host_has_audio: bool,
    /// What the host says it is doing with its output.
    ///
    /// What it **is** doing, not what it was asked to do: a host that cannot
    /// start capture reports the mode it is really in. [`AudioMode::Off`] until
    /// the host has said anything.
    pub mode: AudioMode,
    /// The host sink's volume, in permille of its own hundred percent.
    pub volume: u16,
    /// Whether the host's own sink is muted.
    ///
    /// The host's silence and not the client's. [`AudioMode::Exclusive`] mutes
    /// the host, so this reads true while the client is hearing everything.
    pub muted: bool,
    /// Opus packets that reached the client.
    pub packets: u64,
    /// Frames of sound that never arrived.
    pub lost: u64,
    /// Packets decoded and played.
    pub played: u64,
    /// Frames the decoder invented, because nothing carried the sound.
    pub concealed: u64,
    /// Frames rebuilt out of the packet behind the hole, which a listener heard
    /// rather than had smoothed over.
    pub recovered: u64,
    /// Frames thrown away to hold the play buffer down, which is what corrects
    /// two sound cards running at slightly different rates.
    pub dropped: u64,
    /// Frames waiting to be played right now.
    pub buffered: u32,
}
