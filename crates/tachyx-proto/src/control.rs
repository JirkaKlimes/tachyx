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
use crate::media::{FrameId, Seq, Timestamp};

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
///
/// The order of the variants is the wire encoding: `postcard` writes the
/// discriminant as a varint, so a codec appended here leaves the number of every
/// codec before it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// H.264. The floor of every negotiation: it is what a host with no
    /// hardware newer than 2010 encodes and what a client with no hardware
    /// newer than 2010 decodes, so a session that agrees on nothing else agrees
    /// on this.
    H264,
    /// AV1. A better picture for the same bits, and hardware for it is recent
    /// enough that neither end may assume the other has any. Which is the whole
    /// reason [`Capabilities::decode`] exists.
    Av1,
}

impl Codec {
    /// Every codec this build knows, best first.
    ///
    /// The preference order, and it lives beside the codecs rather than in the
    /// host so that both ends can work out the same answer from the same list.
    /// See [`Codecs::best`].
    pub const PREFERENCE: [Self; 2] = [Self::Av1, Self::H264];

    /// Where this codec sits in a [`Codecs`].
    const fn bit(self) -> u8 {
        match self {
            Self::H264 => 1,
            Self::Av1 => 1 << 1,
        }
    }
}

impl core::fmt::Display for Codec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::H264 => "H.264",
            Self::Av1 => "AV1",
        })
    }
}

/// A set of codecs.
///
/// A bit set rather than a list of [`Codec`], because `postcard` refuses a
/// discriminant it does not know: a list carrying a codec added in a later
/// version fails to decode as a list, and the whole message it travels in fails
/// with it. A bit nobody knows is simply never in an intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Codecs(u8);

impl Codecs {
    /// The empty set, which shares nothing with anything.
    pub const NONE: Self = Self(0);

    /// The set holding one codec.
    #[must_use]
    pub const fn of(codec: Codec) -> Self {
        Self(codec.bit())
    }

    /// This set, and `codec` as well.
    #[must_use]
    pub const fn with(self, codec: Codec) -> Self {
        Self(self.0 | codec.bit())
    }

    /// Whether `codec` is in the set.
    #[must_use]
    pub const fn contains(self, codec: Codec) -> bool {
        self.0 & codec.bit() != 0
    }

    /// What both sets hold.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The best codec in the set, in [`Codec::PREFERENCE`] order.
    ///
    /// `None` for a set holding nothing this build knows, which is what a host
    /// and a client with no codec in common come to.
    #[must_use]
    pub fn best(self) -> Option<Codec> {
        Codec::PREFERENCE
            .into_iter()
            .find(|codec| self.contains(*codec))
    }
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
    /// The codecs this client has a decoder for, probed on the machine it is
    /// running on rather than assumed from the platform.
    ///
    /// The host encodes the best codec this set and its own share, and says
    /// which in [`StreamConfig::codec`]. A set with nothing in common with the
    /// host's ends the session, which is the honest answer: there is no
    /// picture to send.
    ///
    /// Acted on at the hello and only there. A client builds its decoder once
    /// and cannot rebuild it, so the same set travels in every later
    /// [`ClientMessage::ViewportChanged`] and the host must not read a change
    /// into it.
    pub decode: Codecs,
}

impl Capabilities {
    /// The same capabilities, decoding this set.
    ///
    /// What the platform layer that probed the machine's decoders calls, over
    /// capabilities built by a layer that does no I/O and cannot probe.
    #[must_use]
    pub const fn decoding(self, decode: Codecs) -> Self {
        Self { decode, ..self }
    }
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
    /// Frames the host declined because the send buffer had no room for them
    /// whole, or because capture had nowhere to put them.
    ///
    /// A safety interlock rather than a rate control: with the pacer keeping
    /// the transport's queue one or two datagrams deep it should never fire,
    /// and a session in which it does is a session with something else wrong.
    pub frames_gated: u64,
    /// Frames the rate limiter refused because the picture would have cost more
    /// than the link carries.
    ///
    /// Counted apart from `frames_gated` because the two name different faults,
    /// and the session that reported "179 gated" could not say which mechanism
    /// did it.
    pub frames_refused: u64,
    /// The transport's own bandwidth estimate, in bits per second.
    ///
    /// `None` under a congestion controller that does not pace, which is every
    /// controller but BBR. It is a diagnostic and nothing reads it as a target:
    /// measured, it reported 0.00 Mbit/s on a healthy hundred-megabit path for
    /// every sample of a twenty-second run, because its sampler discards any
    /// measurement taken while the sender is limited by its own supply and a
    /// sender producing sixty frames a second always is.
    pub pacing_rate: Option<u64>,
    /// What the host's own congestion controller believes the path will carry,
    /// in bits per second.
    ///
    /// This is the number the session is actually run against, and it is beside
    /// `pacing_rate` rather than instead of it because the difference between
    /// the two is the whole reason this controller exists.
    pub estimate: Option<u32>,
    /// Datagrams of padding released to time the path.
    ///
    /// A still desktop produces no frames, so a controller that only learned
    /// from media would never learn that the link had recovered. These carry no
    /// picture and exist to be timed.
    pub padding_sent: u64,
    /// Median capture to wire latency over the last interval, in microseconds.
    pub pipeline_micros: u32,
}

/// How often the host sends a [`Heartbeat`].
///
/// It is a constant rather than a field of the message because both ends need
/// it and only one of them would carry it: a client reads liveness from the
/// *absence* of a heartbeat, so it has to know the interval before it has ever
/// received one. For the same reason there is one constant and not one at each
/// end, which would be two numbers free to disagree about the same wire fact.
///
/// Four a second. Three missed beats are then noticed three quarters of a
/// second in, well before the transport's own five second idle timeout, which
/// is the next signal after this one and far too late to be the first.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

/// When one video datagram arrived, or that it never did.
///
/// Deltas rather than offsets from the report's reference, because `postcard`
/// varint-encodes and a delta between consecutive datagrams is almost always
/// one byte where an offset would be two. Signed, because reordering is real:
/// Wi-Fi retries and multi-queue hardware both produce it, and an unsigned
/// delta would have to lie about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacketArrival {
    /// Not received when the report was built.
    Missing,
    /// Received this many [`FEEDBACK_TICK`]s after the previous datagram in
    /// this report that arrived. The first arrival is measured from
    /// [`Feedback::reference`], so its delta is zero.
    Received(i16),
}

/// The resolution of every time in a [`Feedback`].
///
/// 250 microseconds is what transport-wide feedback uses, and it is what makes
/// a signed sixteen-bit delta span 8.19 seconds, which is thirty times the
/// longest interval between two reports.
pub const FEEDBACK_TICK: Duration = Duration::from_micros(250);

/// How often a client reports, while datagrams are arriving.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

/// How often a client reports while the host's announced bitrate is under
/// [`FEEDBACK_SLOW_BELOW`].
///
/// libwebrtc computes the interval as 68 bytes of report against 5% of the
/// estimate and clamps it to between 50 and 250 ms. On any link above 218
/// kbit/s that formula returns something under 50 ms and the clamp decides, so
/// carrying the formula would be carrying arithmetic with one answer. The one
/// case where it does something is a collapsing link, and this reproduces it.
pub const FEEDBACK_SLOW_INTERVAL: Duration = Duration::from_millis(250);

/// The announced bitrate under which reports drop to
/// [`FEEDBACK_SLOW_INTERVAL`].
pub const FEEDBACK_SLOW_BELOW: u32 = 250_000;

/// The most datagrams one report accounts for.
///
/// A report describing more than this is split and the parts sent back to back,
/// so nothing is lost by the bound. Measured with real encodings, 512 statuses
/// come to 1041 bytes.
pub const FEEDBACK_MAX_STATUSES: usize = 512;

/// When each video datagram arrived, and which never did.
///
/// **No clock synchronisation is required and none is implied.** Every quantity
/// the host's estimator takes from this is a difference between two of the
/// client's own arrival times, so a constant offset between the two clocks
/// cancels exactly. Rate skew between the two crystals does not cancel, but at
/// the parts per million a real oscillator drifts it is far below the six
/// millisecond floor the delay threshold clamps to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    /// The `seq` of the first datagram this report accounts for.
    pub base: Seq,
    /// The client's own clock at the arrival of the first datagram in
    /// `packets` that arrived, in [`FEEDBACK_TICK`]s since the client's session
    /// began.
    ///
    /// A report in which nothing arrived carries the clock at the moment it was
    /// built, which no estimator reads: with no arrival to measure from there
    /// is nothing to measure.
    pub reference: u32,
    /// One entry per sequence number from `base` upward, in order, no gaps.
    pub packets: Vec<PacketArrival>,
    /// Datagrams the client has never seen, since the session began.
    ///
    /// Cumulative rather than per interval, so that a delayed report is still
    /// exactly right about the interval it covers.
    pub lost: u64,
    /// Datagrams the client has received, since the session began.
    pub received: u64,
}

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
    /// What the path did to the video, which is the only measurement of it the
    /// host can have.
    ///
    /// quinn exposes no per-datagram acknowledgement to the application:
    /// `Connection::on_ack_received` is private, the public event enum carries
    /// nothing about one, and a datagram has no application-visible identity
    /// after it is handed over. So this is not an optimisation of the
    /// transport's own estimate. It is the whole of the host's information.
    Feedback(Feedback),
}

/// Where the host's pointer is and what it looks like.
///
/// The client draws the pointer itself, over the video. Capture deliberately
/// leaves the cursor out of the picture, so this is the only thing that puts it
/// back, and it is a correction rather than a source: a client that waited for
/// one of these before moving its pointer would put a round trip inside every
/// gesture. See [`CursorReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorUpdate {
    /// The compositor reports the pointer.
    Tracked(CursorReport),
    /// The compositor offers no cursor session, so the host cannot say where
    /// the pointer is or what it looks like. Sent once, and nothing follows it.
    ///
    /// A client that is told this draws a pointer of its own at the place its
    /// own input has been steering. That drifts, because nothing corrects it,
    /// and it is still far better than a touch surface with nothing on it.
    Untracked,
}

/// One report of the pointer, in pixels of the captured screen.
///
/// The captured screen, not the encoded picture: the resolution ladder changes
/// the second at any moment and would otherwise change the meaning of every
/// figure here in the middle of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorReport {
    /// The captured screen, which every other figure here is measured in.
    pub screen: Size,
    /// Where the pointer is, or `None` while it is not over the captured
    /// screen.
    pub at: Option<CursorPoint>,
    /// The picture, carried by the update that changes it and by no other.
    ///
    /// It is thousands of times the size of a position and changes thousands of
    /// times less often, and this stream is the same reliable stream input
    /// travels on, so resending it with every position would put the pointer's
    /// own picture in front of the user's next click.
    pub image: Option<CursorImage>,
}

/// Where the pointer's hotspot is, and where that sits inside the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPoint {
    /// Distance from the left edge of the captured screen, in its pixels.
    ///
    /// May fall outside the screen: a cursor overlapping an edge has entered
    /// the captured area while its hotspot has not.
    pub x: i32,
    /// Distance from the top edge.
    pub y: i32,
    /// Where the hotspot sits inside the picture, from its left edge.
    pub hotspot_x: i32,
    /// The same, from its top edge.
    pub hotspot_y: i32,
}

/// A pointer picture, as premultiplied BGRA rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorImage {
    /// Width in pixels of the captured screen.
    pub width: NonZeroU16,
    /// Height.
    pub height: NonZeroU16,
    /// `width * height` pixels, row by row, four bytes each: blue, green, red,
    /// then alpha, with the colour already multiplied by the alpha.
    ///
    /// Arrives from the far end, so [`CursorImage::pixels`] is what to read it
    /// through rather than this field.
    pixels: Vec<u8>,
}

impl CursorImage {
    /// The largest picture one control message carries.
    ///
    /// A control message is capped at 64 KiB and four bytes a pixel is what
    /// this format costs, so this is a cursor of 120 pixels square with room to
    /// spare for the rest of the update. Every cursor theme in use is far
    /// smaller: 48 square is what this host was measured serving.
    pub const MAX_BYTES: usize = 120 * 120 * 4;

    /// Describes a picture, rejecting one whose rows do not add up.
    #[must_use]
    pub fn new(width: NonZeroU16, height: NonZeroU16, pixels: Vec<u8>) -> Option<Self> {
        let image = Self {
            width,
            height,
            pixels,
        };
        image.is_whole().then_some(image)
    }

    /// The rows, once they are known to be the rows this picture claims.
    ///
    /// `None` for a picture that arrived from the far end with the wrong number
    /// of bytes in it, which is the one thing serialisation cannot rule out.
    #[must_use]
    pub fn pixels(&self) -> Option<&[u8]> {
        self.is_whole().then_some(&self.pixels)
    }

    fn is_whole(&self) -> bool {
        let expected = usize::from(self.width.get()) * usize::from(self.height.get()) * 4;
        self.pixels.len() == expected && expected <= Self::MAX_BYTES
    }
}

/// Anything the host sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    /// How the host has decided to encode.
    Stream(StreamConfig),
    /// Proof the host is alive, and what it has been doing.
    Heartbeat(Heartbeat),
    /// Where the pointer is and what it looks like.
    Cursor(CursorUpdate),
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
            decode: Codecs::of(Codec::H264).with(Codec::Av1),
        }
    }

    #[test]
    fn a_codec_set_answers_what_both_ends_can_drive() {
        let client = Codecs::of(Codec::H264).with(Codec::Av1);
        let host = Codecs::of(Codec::H264);
        assert_eq!(client.intersection(host).best(), Some(Codec::H264));
        assert_eq!(client.intersection(client).best(), Some(Codec::Av1));
        assert_eq!(
            Codecs::of(Codec::Av1)
                .intersection(Codecs::of(Codec::H264))
                .best(),
            None,
            "no codec in common is a session that cannot start, not a fallback"
        );
        assert_eq!(Codecs::NONE.best(), None);
        assert!(client.contains(Codec::Av1));
        assert!(!host.contains(Codec::Av1));
    }

    #[test]
    fn a_codec_a_peer_knows_and_this_build_does_not_is_ignored_rather_than_fatal() {
        // The reason the set is a bit set. A later version advertising a third
        // codec must still be understood about the two this one has, and the
        // message it travels in must still parse.
        let later = postcard::to_stdvec(&Capabilities {
            decode: Codecs(0b111),
            ..capabilities()
        })
        .unwrap();
        let read: Capabilities = postcard::from_bytes(&later).unwrap();
        assert_eq!(read.decode.best(), Some(Codec::Av1));
        assert!(read.decode.contains(Codec::H264));
    }

    #[test]
    fn adding_a_codec_leaves_the_ones_before_it_where_they_were() {
        // A `StreamConfig` written by a build that only knew H.264 has to keep
        // reading as H.264 here, which is what appending to the enum buys.
        assert_eq!(postcard::to_stdvec(&Codec::H264).unwrap(), [0]);
        assert_eq!(postcard::to_stdvec(&Codec::Av1).unwrap(), [1]);
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

    /// A report of `count` datagrams with every `nth` one lost, encoded as the
    /// control stream carries it: the message plus its four byte length prefix.
    fn report_bytes(count: usize, nth: usize) -> usize {
        let packets = (0..count)
            .map(|index| {
                if nth > 0 && index.is_multiple_of(nth) {
                    PacketArrival::Missing
                } else {
                    PacketArrival::Received(3)
                }
            })
            .collect();
        let message = ClientMessage::Feedback(Feedback {
            base: Seq(40_000),
            reference: 900_000,
            packets,
            lost: 24,
            received: 859,
        });
        postcard::to_stdvec(&message).unwrap().len() + 4
    }

    #[test]
    fn a_report_costs_about_two_bytes_a_datagram() {
        // The reason the deltas are deltas. At 20 Mbit/s a 50 ms report covers
        // 88 datagrams, so this decides whether feedback is a fifth of a
        // percent of the link or two percent of it.
        for (datagrams, ceiling) in [(18, 56), (88, 200), (512, 1060)] {
            let bytes = report_bytes(datagrams, 0);
            assert!(
                bytes <= ceiling,
                "a report of {datagrams} datagrams took {bytes} bytes"
            );
        }
        assert!(
            report_bytes(88, 20) < report_bytes(88, 0),
            "a missing datagram costs less than one that arrived, not more"
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
            frames_refused: 9,
            pacing_rate: Some(940_000_000),
            estimate: Some(8_500_000),
            padding_sent: 15,
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
            ClientMessage::Feedback(Feedback {
                base: Seq(65_530),
                reference: 1_234_567,
                packets: vec![
                    PacketArrival::Received(0),
                    PacketArrival::Received(3),
                    PacketArrival::Missing,
                    PacketArrival::Received(-2),
                ],
                lost: 24,
                received: 859,
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

    fn edge(size: u16) -> NonZeroU16 {
        NonZeroU16::new(size).unwrap()
    }

    fn arrow() -> CursorImage {
        CursorImage::new(edge(48), edge(48), vec![0x7f; 48 * 48 * 4]).unwrap()
    }

    #[test]
    fn a_cursor_update_survives_the_round_trip() {
        let messages = [
            HostMessage::Cursor(CursorUpdate::Untracked),
            HostMessage::Cursor(CursorUpdate::Tracked(CursorReport {
                screen: Size::new(3440, 1440).unwrap(),
                at: Some(CursorPoint {
                    x: 1436,
                    y: 710,
                    hotspot_x: 5,
                    hotspot_y: 1,
                }),
                image: Some(arrow()),
            })),
            HostMessage::Cursor(CursorUpdate::Tracked(CursorReport {
                screen: Size::new(3440, 1440).unwrap(),
                at: None,
                image: None,
            })),
        ];
        for message in messages {
            let bytes = postcard::to_stdvec(&message).unwrap();
            assert_eq!(
                postcard::from_bytes::<HostMessage>(&bytes).unwrap(),
                message
            );
        }
    }

    #[test]
    fn a_position_costs_a_fraction_of_the_picture_it_carries_without() {
        let screen = Size::new(3440, 1440).unwrap();
        let at = Some(CursorPoint {
            x: 1436,
            y: 710,
            hotspot_x: 5,
            hotspot_y: 1,
        });
        let moved =
            postcard::to_stdvec(&HostMessage::Cursor(CursorUpdate::Tracked(CursorReport {
                screen,
                at,
                image: None,
            })))
            .unwrap();
        let redrawn =
            postcard::to_stdvec(&HostMessage::Cursor(CursorUpdate::Tracked(CursorReport {
                screen,
                at,
                image: Some(arrow()),
            })))
            .unwrap();
        assert!(
            moved.len() < 24,
            "a position that carries no picture must stay small, was {} bytes",
            moved.len()
        );
        assert!(
            redrawn.len() > 100 * moved.len(),
            "the picture is what this message avoids resending, and it is {} bytes",
            redrawn.len()
        );
    }

    #[test]
    fn a_picture_whose_rows_do_not_add_up_is_refused() {
        assert!(
            CursorImage::new(edge(48), edge(48), vec![0; 48 * 48 * 4 - 1]).is_none(),
            "a short buffer would be read past its end by whatever draws it"
        );
        assert!(
            CursorImage::new(edge(512), edge(512), vec![0; 512 * 512 * 4]).is_none(),
            "a picture larger than a control message cannot be sent"
        );
    }

    #[test]
    fn a_picture_that_arrives_malformed_reads_as_nothing() {
        // What a peer that lies about its own rows puts on the wire. Nothing in
        // the encoding rules it out, so the reader has to.
        #[derive(Serialize)]
        struct Claimed {
            width: u16,
            height: u16,
            pixels: Vec<u8>,
        }

        let whole = arrow();
        assert_eq!(whole.pixels().map(<[u8]>::len), Some(48 * 48 * 4));
        let bytes = postcard::to_stdvec(&Claimed {
            width: 48,
            height: 48,
            pixels: vec![0; 16],
        })
        .unwrap();
        let lie: CursorImage = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(lie.pixels(), None);
    }
}
