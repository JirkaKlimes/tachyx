//! Screen capture and hardware video encode for the tachyx host.
//!
//! Two threads and one slot between them.
//!
//! The capture thread owns a Wayland connection, a ring of three shared buffers
//! and a `ext-image-copy-capture-v1` session. When a buffer becomes ready it
//! arms the next one immediately and only then publishes the finished one, so a
//! capture request is outstanding at every vertical blank.
//!
//! The encode thread takes from a depth-one slot and drives GStreamer. Pushing
//! a buffer through the scale and encode chain costs about the length of a
//! refresh period, so the two jobs cannot share a thread: one that captured,
//! pushed and sent in turn would serialise into half the refresh rate.
//!
//! The slot holds one frame. Storing into a full slot replaces what is there and
//! counts a drop. It is not a queue: a frame that the encoder could not start on
//! before the next one arrived is stale, and delivering it late would cost
//! latency on every frame after it.
//!
//! Three buffers rather than two. With two, a buffer that the encoder still
//! holds when the next one becomes ready leaves the capture thread with nothing
//! to arm, and capture stalls until the encoder returns.
//!
//! The cursor is not in the picture. The compositor leaves it out unless asked,
//! and a second session on the same connection reports where it is, what its
//! hotspot is and what it looks like. That is what lets a client draw the
//! pointer itself and move it the instant the finger does, rather than watching
//! it arrive one whole pipeline late. [`Capture::cursor`] is where it comes out.

mod capture;
mod encode;
mod slot;

use core::num::NonZeroU32;

pub use capture::{
    Capture, CaptureFormat, CaptureStats, Cursor, CursorImage, Frame, Frames, Plane, Pointer,
};
pub use encode::{
    AccessUnit, AccessUnits, EncodeConfig, EncodeStats, Encoder, Idle, Percentiles, StreamGeometry,
    encoders,
};

/// Why capture or encode could not be set up or carried on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The compositor could not be reached.
    #[error("cannot connect to the wayland compositor")]
    Connect(#[from] wayland_client::ConnectError),
    /// The connection to the compositor failed.
    #[error("wayland connection lost")]
    Dispatch(#[from] wayland_client::DispatchError),
    /// A request to the compositor could not be delivered.
    #[error("wayland request failed")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    /// The compositor does not offer a protocol that capture requires.
    #[error("compositor does not offer {0}, which capture requires")]
    Missing(&'static str),
    /// The compositor announced no output by the requested name.
    #[error("no output named {0}")]
    NoSuchOutput(String),
    /// The compositor ended the capture session.
    #[error("the compositor stopped the capture session")]
    SessionStopped,
    /// The output changed shape, so the buffers allocated for it no longer fit.
    #[error("the compositor changed the capture constraints")]
    ConstraintsChanged,
    /// The compositor named a device that is not present under `/dev/dri`.
    #[error("no /dev/dri node has device number {0}")]
    NoRenderNode(u64),
    /// No pixel format and buffer layout is acceptable to both the compositor
    /// and the encoder.
    #[error("the compositor and the encoder share no buffer layout")]
    NoSharedFormat,
    /// The allocator refused to export a shared descriptor for a buffer.
    #[error("the allocator would not export a shared descriptor")]
    NoDescriptor,
    /// A plane of one buffer object came from a different shared descriptor
    /// than the others, which the single-descriptor import cannot express.
    #[error("the allocator split one buffer object across several descriptors")]
    SplitBufferObject,
    /// GStreamer does not have an element the pipeline needs.
    #[error("gstreamer has no {0} element")]
    MissingElement(&'static str),
    /// This machine has no encoder for the codec the session settled on.
    ///
    /// A negotiation that got this far chose from what [`encoders`] reported,
    /// so reaching here means the registry answered one thing and the element
    /// factory another.
    #[error("this machine has no {0} encoder")]
    MissingEncoder(tachyx_proto::control::Codec),
    /// GStreamer reported a failure.
    #[error("gstreamer: {0}")]
    Gst(String),
    /// The encoder produced no access unit for a frame that was pushed.
    #[error("the encoder produced nothing for a frame within {0:?}")]
    EncodeTimeout(core::time::Duration),
    /// A file or device could not be opened, read or allocated from.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<gstreamer::glib::BoolError> for Error {
    fn from(error: gstreamer::glib::BoolError) -> Self {
        Self::Gst(error.message.into())
    }
}

impl From<gstreamer::glib::Error> for Error {
    fn from(error: gstreamer::glib::Error) -> Self {
        Self::Gst(error.to_string())
    }
}

impl From<gbm::InvalidFdError> for Error {
    fn from(_: gbm::InvalidFdError) -> Self {
        Self::NoDescriptor
    }
}

impl From<gstreamer::StateChangeError> for Error {
    fn from(_: gstreamer::StateChangeError) -> Self {
        Self::Gst("the pipeline refused to change state".to_owned())
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

/// A picture size in pixels, neither dimension zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl Size {
    /// A size, rejecting a zero dimension.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width.get()
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height.get()
    }
}

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A size every encoder this host may negotiate onto will accept.
///
/// Odd dimensions cannot be coded in 4:2:0 at all, and the hardware refuses
/// anything outside 128 to 4096. Both are checked once, here, so the pipeline
/// never has to ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeSize(Size);

/// Why a requested encode size is not codeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BadEncodeSize {
    /// Chroma is subsampled by two in both directions, so an odd dimension has
    /// no representation.
    #[error("an encoded dimension must be even")]
    Odd,
    /// Outside the range the encoder's pad template advertises.
    #[error("an encoded dimension must be between {min} and {max}")]
    OutOfRange {
        /// Smallest dimension the encoder accepts.
        min: u32,
        /// Largest dimension the encoder accepts.
        max: u32,
    },
}

impl EncodeSize {
    /// Smallest dimension the VA encoders advertise on their sink pads.
    pub const MIN: u32 = 128;
    /// Largest dimension every VA encoder advertises on its sink pad.
    ///
    /// The narrowest of them, because the codec is chosen per session and a
    /// size accepted by one and refused by another would be a ladder that works
    /// until the negotiation changes. Measured on this part: `vah264enc` takes
    /// 128 to 4096 in both axes and `vaav1enc` takes 128 to 8192 by 128 to
    /// 4352, so this is H.264's ceiling.
    pub const MAX: u32 = 4096;

    /// A size to encode at.
    ///
    /// # Errors
    /// If either dimension is odd or falls outside the encoder's range.
    pub fn new(width: u32, height: u32) -> Result<Self, BadEncodeSize> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(BadEncodeSize::Odd);
        }
        let range = Self::MIN..=Self::MAX;
        if !range.contains(&width) || !range.contains(&height) {
            return Err(BadEncodeSize::OutOfRange {
                min: Self::MIN,
                max: Self::MAX,
            });
        }
        Size::new(width, height)
            .map(Self)
            .ok_or(BadEncodeSize::OutOfRange {
                min: Self::MIN,
                max: Self::MAX,
            })
    }

    /// The largest size no taller than `height` that keeps `source`'s shape and
    /// lands on a multiple of sixteen in both directions.
    ///
    /// Sixteen because the Android presentation path ignores the crop rectangle
    /// that H.264 uses to express a size the macroblock grid cannot hold, and
    /// the same shape because the scaler's aspect correction never reaches the
    /// bitstream.
    ///
    /// # Errors
    /// If the result would fall outside the encoder's range.
    pub fn fit(source: Size, height: u32) -> Result<Self, BadEncodeSize> {
        // Rounded to even rather than to a macroblock. The crop that H.264
        // attaches is expressed in chroma samples, which are half a pixel apart
        // in each axis under 4:2:0, so even is the real constraint. Forcing a
        // multiple of sixteen here would distort the picture to avoid a crop
        // that costs nothing, and `coded` already accounts for the macroblocks.
        let height = (height / 2) * 2;
        let width = u64::from(source.width()) * u64::from(height) / u64::from(source.height());
        let width = u32::try_from(width).unwrap_or(Self::MAX) / 2 * 2;
        Self::new(width, height)
    }

    /// The size a decoder must show.
    #[must_use]
    pub const fn visible(self) -> Size {
        self.0
    }

    /// The size the bitstream actually codes.
    ///
    /// H.264 codes whole macroblocks, so 1080 travels as 1088 rows with eight
    /// of them cropped away and 1800 travels as 1808.
    ///
    /// # Panics
    /// If the visible size is within fifteen pixels of the encoder's ceiling in
    /// either axis, which [`EncodeSize::new`] rejects.
    #[must_use]
    pub fn coded(self) -> Size {
        let round = |value: u32| value.div_ceil(16) * 16;
        Size::new(round(self.0.width()), round(self.0.height()))
            .expect("rounding up never reaches zero")
    }

    /// Width to height of the picture as it must appear, in lowest terms.
    ///
    /// The scaler letterboxes rather than stretching, so pixels are square and
    /// this is the visible size reduced.
    #[must_use]
    pub fn display_aspect(self) -> (u32, u32) {
        let (mut a, mut b) = (self.0.width(), self.0.height());
        while b != 0 {
            (a, b) = (b, a % b);
        }
        (self.0.width() / a, self.0.height() / a)
    }
}

/// A frame rate, in millihertz.
///
/// The encoder budgets bits per frame as the bitrate divided by the frame rate
/// declared in the caps, and ignores presentation timestamps entirely, so this
/// is half of the rate control and never a display hint. Millihertz because a
/// panel's mode is reported that way and a nominal 120 is often 119.998.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framerate(NonZeroU32);

impl Framerate {
    /// A frame rate from a mode line, in thousandths of a hertz.
    #[must_use]
    pub const fn from_millihertz(millihertz: u32) -> Option<Self> {
        match NonZeroU32::new(millihertz) {
            Some(rate) => Some(Self(rate)),
            None => None,
        }
    }

    /// A whole number of frames per second.
    #[must_use]
    pub const fn from_hertz(hertz: u32) -> Option<Self> {
        Self::from_millihertz(hertz.saturating_mul(1000))
    }

    /// The rate in thousandths of a hertz.
    #[must_use]
    pub const fn millihertz(self) -> u32 {
        self.0.get()
    }

    pub(crate) fn as_fraction(self) -> gstreamer::Fraction {
        gstreamer::Fraction::new(i32::try_from(self.0.get()).unwrap_or(i32::MAX), 1000)
    }
}

impl core::fmt::Display for Framerate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:03} Hz", self.0.get() / 1000, self.0.get() % 1000)
    }
}

/// A target bitrate, in kilobits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bitrate(NonZeroU32);

impl Bitrate {
    /// Largest value the encoder's property accepts.
    pub const MAX_KBPS: u32 = 2_048_000;

    /// A bitrate in kilobits per second, clamped to what the encoder accepts.
    #[must_use]
    pub const fn from_kbps(kbps: u32) -> Option<Self> {
        let kbps = if kbps > Self::MAX_KBPS {
            Self::MAX_KBPS
        } else {
            kbps
        };
        match NonZeroU32::new(kbps) {
            Some(rate) => Some(Self(rate)),
            None => None,
        }
    }

    /// The rate in kilobits per second.
    #[must_use]
    pub const fn kbps(self) -> u32 {
        self.0.get()
    }
}

impl core::fmt::Display for Bitrate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} kbit/s", self.0.get())
    }
}

/// Bitrate and declared frame rate, which are one setting rather than two.
///
/// The encoder spends `bitrate / framerate` on every picture whatever the
/// delivery cadence, so changing one without the other moves the real rate by
/// the ratio between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Bits per second to aim for.
    pub bitrate: Bitrate,
    /// Frames per second the bitrate is divided by. Track the output's refresh
    /// rate rather than a nominal figure.
    pub framerate: Framerate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encode_size_rejects_what_the_encoder_cannot_code() {
        assert_eq!(EncodeSize::new(1281, 720), Err(BadEncodeSize::Odd));
        assert!(matches!(
            EncodeSize::new(126, 720),
            Err(BadEncodeSize::OutOfRange { .. })
        ));
        assert!(matches!(
            EncodeSize::new(5120, 2880),
            Err(BadEncodeSize::OutOfRange { .. })
        ));
        assert!(EncodeSize::new(1280, 720).is_ok());
    }

    #[test]
    fn the_coded_size_covers_the_visible_one_in_whole_macroblocks() {
        let size = EncodeSize::new(1920, 1080).unwrap();
        assert_eq!(size.coded(), Size::new(1920, 1088).unwrap());
        assert_eq!(size.visible(), Size::new(1920, 1080).unwrap());
        assert_eq!(
            EncodeSize::new(2880, 1800).unwrap().coded(),
            Size::new(2880, 1808).unwrap()
        );
        assert_eq!(
            EncodeSize::new(1280, 720).unwrap().coded(),
            Size::new(1280, 720).unwrap()
        );
    }

    #[test]
    fn fitting_keeps_the_shape_of_the_capture() {
        let source = Size::new(2880, 1800).unwrap();
        let fitted = EncodeSize::fit(source, 1080).unwrap();
        assert_eq!(fitted.visible(), Size::new(1728, 1080).unwrap());
        assert_eq!(
            fitted.display_aspect(),
            source_aspect(source),
            "a fitted size that changes the shape would make the scaler stretch"
        );
        assert_eq!(fitted.coded(), Size::new(1728, 1088).unwrap());
    }

    #[test]
    fn a_fitted_size_is_always_even_in_both_axes() {
        let source = Size::new(2880, 1800).unwrap();
        for height in [1080u32, 900, 810, 722, 540, 400] {
            let fitted = EncodeSize::fit(source, height).unwrap();
            assert_eq!(fitted.visible().width() % 2, 0, "height {height}");
            assert_eq!(fitted.visible().height() % 2, 0, "height {height}");
        }
    }

    fn source_aspect(source: Size) -> (u32, u32) {
        EncodeSize::new(source.width(), source.height())
            .unwrap()
            .display_aspect()
    }

    #[test]
    fn a_display_aspect_is_reduced() {
        assert_eq!(
            EncodeSize::new(1920, 1080).unwrap().display_aspect(),
            (16, 9)
        );
        assert_eq!(EncodeSize::new(1280, 800).unwrap().display_aspect(), (8, 5));
    }
}
