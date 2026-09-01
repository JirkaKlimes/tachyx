//! Messages on the reliable control stream.
//!
//! Every message is an ordinary Rust type serialised with `postcard`. That
//! encoding is not self-describing, so the first message in each direction
//! carries [`VERSION`] ahead of everything else, and [`Hello::decode`] reads
//! that field before it reads anything a later version might have reshaped.

use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::VERSION;
use crate::input::InputEvent;
use crate::media::{FrameId, Timestamp};

/// A picture size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in pixels.
    pub width: NonZeroU16,
    /// Height in pixels.
    pub height: NonZeroU16,
}

impl Size {
    /// Describes a size, rejecting one with no area.
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Option<Self> {
        match (NonZeroU16::new(width), NonZeroU16::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }

    /// Pixels the picture holds.
    #[must_use]
    pub const fn pixels(self) -> u32 {
        self.width.get() as u32 * self.height.get() as u32
    }
}

/// A size the encoder codes without attaching a crop rectangle.
///
/// H.264 codes a height of 1080 as 1088 and expresses the difference as a crop,
/// and Flutter's Android presentation path does not apply that crop. Every rung
/// of the resolution ladder is therefore a multiple of sixteen, which avoids the
/// problem rather than detecting it, and this type is what makes a rung that is
/// not one impossible to put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodedSize(Size);

impl CodedSize {
    /// The alignment H.264 macroblocks and the client's presentation path
    /// between them require.
    pub const ALIGNMENT: u16 = 16;

    /// Describes a coded size, rejecting one the client cannot present.
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Option<Self> {
        if !width.is_multiple_of(Self::ALIGNMENT) || !height.is_multiple_of(Self::ALIGNMENT) {
            return None;
        }
        match Size::new(width, height) {
            Some(size) => Some(Self(size)),
            None => None,
        }
    }

    /// The size, once it is known to be codeable.
    #[must_use]
    pub const fn size(self) -> Size {
        self.0
    }
}

/// The shape a picture is meant to be drawn at, as width over height.
///
/// It travels here because it cannot be recovered from the bitstream: the
/// sequence parameter set of a 16:10 capture scaled to 16:9 declares no aspect
/// ratio at all, so the scaler's correction never reaches the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aspect {
    /// The width part of the ratio.
    pub width: NonZeroU16,
    /// The height part of the ratio.
    pub height: NonZeroU16,
}

impl Aspect {
    /// Describes a ratio, rejecting one with a zero term.
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Option<Self> {
        match (NonZeroU16::new(width), NonZeroU16::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }
}

/// A video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// H.264. The only codec the host's hardware encoder is driven with.
    H264,
}

/// What a client can decode, and how much of it it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The video surface, in physical pixels.
    pub viewport: Size,
    /// The largest picture the client's decoder accepts.
    pub max_decode: Size,
    /// The highest frame rate that decoder sustains at `max_decode`.
    pub max_decode_fps: NonZeroU16,
    /// The frame rate the client would like, which may be lower than it can
    /// decode: a phone on battery asks for less than it is able to take.
    pub requested_fps: NonZeroU16,
    /// The most the client will let the stream cost, in bits per second.
    pub bitrate_ceiling: NonZeroU32,
}

/// A first message, which carries the protocol version ahead of everything
/// else.
///
/// Both ends send one and both check the other's. Checking in one direction
/// only leaves the end that did not check decoding a message a later version
/// reshaped: `postcard` writes neither field names nor framing, so those bytes
/// do not fail to parse, they parse into something else.
pub trait Hello: Sized {
    /// Reads the message, checking the version before anything after it.
    ///
    /// # Errors
    /// If the version does not match this build, or the bytes are not this
    /// message.
    fn decode(bytes: &[u8]) -> Result<Self, HandshakeError>;
}

/// Reads a message whose first field is the protocol version, and refuses it by
/// name before a mismatch can be decoded into plausible nonsense.
fn decode_versioned<M: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<M, HandshakeError> {
    let (version, _) =
        postcard::take_from_bytes::<u16>(bytes).map_err(|_| HandshakeError::Malformed)?;
    if version != VERSION {
        return Err(HandshakeError::Version { theirs: version });
    }
    postcard::from_bytes(bytes).map_err(|_| HandshakeError::Malformed)
}

/// The client's first message on the control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// Read before anything after it. Private so that a hello cannot be built
    /// claiming a version this build does not speak.
    version: u16,
    /// What the client can drive.
    pub capabilities: Capabilities,
}

impl ClientHello {
    /// Announces this build's protocol version and the client's capabilities.
    #[must_use]
    pub const fn new(capabilities: Capabilities) -> Self {
        Self {
            version: VERSION,
            capabilities,
        }
    }

    /// The version the peer claims.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }
}

impl Hello for ClientHello {
    fn decode(bytes: &[u8]) -> Result<Self, HandshakeError> {
        decode_versioned(bytes)
    }
}

/// The host's answer to a [`ClientHello`], and the first thing it says.
///
/// It carries the version and nothing else. There is nothing else for the host
/// to announce at this point: the host, not the client, decides how to encode,
/// and it says so in [`StreamConfig`] once it has read what the client can
/// drive. What this message exists for is the version, which the client
/// otherwise has no way to check.
///
/// The host sends it before it has read the client's hello, so the order the
/// two arrive in never matters. It is not what a refused client usually reads,
/// though: closing a connection discards stream data the peer has not
/// acknowledged, so the reason for a refusal reaches the client as the
/// connection's close reason, which is measured, not assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    /// Read before anything after it, and private for the same reason as
    /// [`ClientHello::version`].
    version: u16,
}

impl HostHello {
    /// Announces this build's protocol version.
    #[must_use]
    pub const fn new() -> Self {
        Self { version: VERSION }
    }

    /// The version the peer claims.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }
}

impl Default for HostHello {
    fn default() -> Self {
        Self::new()
    }
}

impl Hello for HostHello {
    fn decode(bytes: &[u8]) -> Result<Self, HandshakeError> {
        decode_versioned(bytes)
    }
}

/// Why a session cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The peer speaks a protocol this build does not.
    #[error("peer speaks protocol version {theirs}, this build speaks {VERSION}")]
    Version {
        /// The version the peer claims.
        theirs: u16,
    },
    /// The first message is not a hello at all.
    #[error("the first control message is not a client hello")]
    Malformed,
}

/// One input event and when the client saw the user produce it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedInput {
    /// The client's session clock at the moment the user acted.
    pub at: Timestamp,
    /// What the user did.
    pub event: InputEvent,
}

/// Input events batched on a short tick, so that a burst of pointer motion
/// costs one packet rather than one packet per sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputBatch {
    /// In the order the user produced them.
    pub events: Vec<TimedInput>,
}

/// How the host has decided to encode.
///
/// Re-sent whenever any of it changes, including a change forced by congestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamConfig {
    /// The codec the bitstream is in.
    pub codec: Codec,
    /// The size the encoder codes.
    pub coded: CodedSize,
    /// The part of the coded picture to show.
    pub visible: Size,
    /// The shape to draw the visible part at.
    pub display_aspect: Aspect,
    /// The frame rate declared to the encoder.
    ///
    /// Not a hint. The encoder budgets bits per frame as the bitrate divided by
    /// this and ignores presentation timestamps entirely, so it is half of the
    /// bitrate setting rather than a separate one.
    pub framerate: NonZeroU16,
    /// The target bitrate, in bits per second.
    pub bitrate: NonZeroU32,
}

/// What the host is doing, sent on a fixed interval whether or not frames flow.
///
/// The host must say this out loud. Capture blocks while the screen is static,
/// with idle gaps measured up to 1.53 seconds, so silence on the video path
/// does not distinguish an idle host from a dead one. QUIC's own keep-alive is
/// a transport ping the application never observes, so it does not answer the
/// question either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The host's session clock when this was sent.
    pub sent: Timestamp,
    /// The most recent frame the host put on the wire, if any.
    pub last_frame: Option<FrameId>,
    /// Frames sent this session.
    pub frames_sent: u64,
    /// Frames the host declined to encode because the send buffer had no room
    /// for them whole.
    pub frames_gated: u64,
    /// The transport's own bandwidth estimate, in bits per second.
    ///
    /// `None` under a congestion controller that does not pace, which is every
    /// controller but BBR.
    pub pacing_rate: Option<u64>,
    /// Median capture to wire latency over the last interval, in microseconds.
    pub pipeline_micros: u32,
}

/// How often the host sends a [`Heartbeat`].
///
/// It is a constant rather than a field of the message because both ends need
/// it and only one of them would carry it: a client reads liveness from the
/// *absence* of a heartbeat, so it has to know the interval before it has
/// received one.
///
/// Twice the transport's keep-alive, which is what makes the connection itself
/// hold open, and short enough that three missed beats are noticed inside the
/// five-second idle timeout that would otherwise be the only signal.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Anything a client sends after its hello.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// The client's capabilities changed: a rotation, a resize, or a new
    /// resolution mode. The host renegotiates.
    ViewportChanged(Capabilities),
    /// Input the user produced since the last batch.
    Input(InputBatch),
    /// Reassembly failed and the client cannot continue from what it holds.
    KeyframeRequest {
        /// The last frame the client decoded, or `None` if it has decoded none.
        after: Option<FrameId>,
    },
}

/// Anything the host sends.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    /// How the host has decided to encode.
    Stream(StreamConfig),
    /// Proof the host is alive, and what it has been doing.
    Heartbeat(Heartbeat),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> Capabilities {
        Capabilities {
            viewport: Size::new(2400, 1080).unwrap(),
            max_decode: Size::new(3840, 2160).unwrap(),
            max_decode_fps: NonZeroU16::new(60).unwrap(),
            requested_fps: NonZeroU16::new(60).unwrap(),
            bitrate_ceiling: NonZeroU32::new(20_000_000).unwrap(),
        }
    }

    #[test]
    fn a_hello_survives_the_round_trip() {
        let hello = ClientHello::new(capabilities());
        let bytes = postcard::to_stdvec(&hello).unwrap();
        assert_eq!(ClientHello::decode(&bytes), Ok(hello));
    }

    #[test]
    fn a_host_hello_survives_the_round_trip() {
        let hello = HostHello::new();
        assert_eq!(hello.version(), VERSION);
        let bytes = postcard::to_stdvec(&hello).unwrap();
        assert_eq!(HostHello::decode(&bytes), Ok(hello));
    }

    #[test]
    fn a_version_mismatch_is_named_rather_than_misparsed() {
        #[derive(Serialize)]
        struct Later {
            version: u16,
            anything: [u8; 3],
        }
        let bytes = postcard::to_stdvec(&Later {
            version: VERSION + 7,
            anything: [1, 2, 3],
        })
        .unwrap();
        let mismatch = HandshakeError::Version {
            theirs: VERSION + 7,
        };
        // Both directions, because a client that cannot check the host's
        // version decodes whatever a later host sends into something that looks
        // like a message of its own.
        assert_eq!(ClientHello::decode(&bytes), Err(mismatch));
        assert_eq!(HostHello::decode(&bytes), Err(mismatch));
    }

    #[test]
    fn a_truncated_hello_is_not_a_version_mismatch() {
        assert_eq!(ClientHello::decode(&[]), Err(HandshakeError::Malformed));
        let bytes = postcard::to_stdvec(&ClientHello::new(capabilities())).unwrap();
        assert_eq!(
            ClientHello::decode(&bytes[..bytes.len() - 2]),
            Err(HandshakeError::Malformed)
        );
    }

    #[test]
    fn a_coded_size_the_client_cannot_present_cannot_be_built() {
        assert!(
            CodedSize::new(1920, 1080).is_none(),
            "1080 is coded as 1088"
        );
        assert!(CodedSize::new(1920, 1088).is_some());
        assert!(CodedSize::new(1280, 720).is_some());
        assert!(CodedSize::new(0, 720).is_none());
    }

    #[test]
    fn every_message_survives_the_round_trip() {
        let stream = HostMessage::Stream(StreamConfig {
            codec: Codec::H264,
            coded: CodedSize::new(1920, 1088).unwrap(),
            visible: Size::new(1920, 1080).unwrap(),
            display_aspect: Aspect::new(16, 9).unwrap(),
            framerate: NonZeroU16::new(60).unwrap(),
            bitrate: NonZeroU32::new(8_000_000).unwrap(),
        });
        let heartbeat = HostMessage::Heartbeat(Heartbeat {
            sent: Timestamp::from_session_micros(1_234_500),
            last_frame: Some(FrameId(42)),
            frames_sent: 42,
            frames_gated: 1,
            pacing_rate: Some(940_000_000),
            pipeline_micros: 7300,
        });
        for message in [stream, heartbeat] {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(
                postcard::from_bytes::<HostMessage>(&bytes).unwrap(),
                message
            );
        }

        let messages = [
            ClientMessage::ViewportChanged(capabilities()),
            ClientMessage::KeyframeRequest {
                after: Some(FrameId(9)),
            },
            ClientMessage::Input(InputBatch {
                events: vec![TimedInput {
                    at: Timestamp::from_session_micros(500),
                    event: InputEvent::PointerMotion { dx: 3.0, dy: -1.5 },
                }],
            }),
        ];
        for message in messages {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(
                postcard::from_bytes::<ClientMessage>(&bytes).unwrap(),
                message
            );
        }
    }
}
