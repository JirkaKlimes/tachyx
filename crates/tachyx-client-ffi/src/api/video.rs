//! What the decode and present path counts, reported to Dart.
//!
//! Dart never sees a frame. It gets a texture identifier to put in a `Texture`
//! widget and hands encoded access units the other way, so the picture goes from
//! the decoder to the compositor without crossing this boundary at all. These
//! figures are all that does cross it.

/// A latency distribution over the last few hundred frames, in microseconds.
#[derive(Debug, Clone, Copy)]
pub struct Latency {
    /// The fastest frame in the window.
    pub min_us: u32,
    /// The middle frame in the window.
    pub median_us: u32,
    /// The frame at the ninety-fifth percentile of the window.
    pub p95_us: u32,
    /// The slowest frame in the window.
    pub max_us: u32,
}

impl From<tachyx_video::Latency> for Latency {
    fn from(latency: tachyx_video::Latency) -> Self {
        Self {
            min_us: latency.min_us,
            median_us: latency.median_us,
            p95_us: latency.p95_us,
            max_us: latency.max_us,
        }
    }
}

/// What the present path has counted. Every figure is measured here.
#[derive(Debug, Clone, Copy)]
pub struct PresentStats {
    /// Access units handed to the decoder.
    pub pushed: u64,
    /// Frames the decoder produced as textures.
    pub decoded: u64,
    /// Frames handed to Flutter's rasteriser.
    pub presented: u64,
    /// Frames a newer one replaced before Flutter asked for a texture. This is
    /// the decoder running ahead of the display, not a loss.
    pub superseded: u64,
    /// From handing over an access unit to the frame leaving the decoder.
    pub decode: Latency,
    /// From handing over an access unit to the frame reaching the rasteriser.
    pub present: Latency,
}

impl From<tachyx_video::Stats> for PresentStats {
    fn from(stats: tachyx_video::Stats) -> Self {
        Self {
            pushed: stats.pushed,
            decoded: stats.decoded,
            presented: stats.presented,
            superseded: stats.superseded,
            decode: stats.decode.into(),
            present: stats.present.into(),
        }
    }
}
