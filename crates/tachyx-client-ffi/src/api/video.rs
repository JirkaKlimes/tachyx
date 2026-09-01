//! The decode and present path, reached from Dart.
//!
//! Dart never sees a frame. It gets a texture identifier to put in a `Texture`
//! widget and hands encoded access units the other way, so the picture goes from
//! the decoder to the compositor without crossing this boundary at all.

use flutter_rust_bridge::frb;

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

/// A decoded stream, drawn by a Flutter `Texture` widget.
///
/// Holds [`tachyx_video::VideoTexture`] and adds nothing.
#[frb(opaque)]
#[derive(Debug)]
pub struct VideoTexture {
    core: tachyx_video::VideoTexture,
}

impl VideoTexture {
    /// The identifier to give a `Texture` widget.
    #[frb(sync)]
    #[must_use]
    pub fn id(&self) -> i64 {
        self.core.id()
    }

    /// Hands one encoded access unit to the decoder.
    ///
    /// Synchronous, because a round trip through the isolate for every frame
    /// would put the thing being measured inside the measurement.
    ///
    /// # Errors
    /// If the decoder rejects the access unit.
    #[frb(sync)]
    pub fn push(&self, access_unit: Vec<u8>) -> anyhow::Result<()> {
        self.core.push(access_unit)?;
        Ok(())
    }

    /// Tells the decoder the stream has ended.
    #[frb(sync)]
    pub fn finish(&self) {
        self.core.finish();
    }

    /// What the path has counted so far.
    #[frb(sync)]
    #[must_use]
    pub fn stats(&self) -> PresentStats {
        let stats = self.core.stats();
        PresentStats {
            pushed: stats.pushed,
            decoded: stats.decoded,
            presented: stats.presented,
            superseded: stats.superseded,
            decode: stats.decode.into(),
            present: stats.present.into(),
        }
    }
}

/// Registers a texture with the engine.
///
/// `engine_handle` comes from the `irondash_engine_context` Dart plugin.
/// Synchronous so that it runs on the platform thread, which is the only thread
/// that can reach the engine's texture registry.
///
/// # Errors
/// If GStreamer will not start, a decoder element is missing, or the engine
/// refuses the texture.
#[frb(sync)]
pub fn open_video_texture(engine_handle: i64) -> anyhow::Result<VideoTexture> {
    Ok(VideoTexture {
        core: tachyx_video::VideoTexture::new(engine_handle)?,
    })
}
