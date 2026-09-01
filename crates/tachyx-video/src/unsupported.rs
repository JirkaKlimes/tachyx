//! The same names on a target that does not have this path.
//!
//! Android decodes with the platform decoder into a surface producer, which
//! shares no code with the pipeline here. Naming the types anyway keeps one
//! shape at the Dart boundary, so a caller gets an error that says what is
//! missing instead of a link that fails.

/// Why the present path could not be built or run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This target presents video some other way.
    #[error("this target has no OpenGL texture present path; it uses a surface producer")]
    WrongTarget,
}

/// A latency distribution, in microseconds. Never filled in here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

/// What the present path has done. Never filled in here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Access units handed to the decoder.
    pub pushed: u64,
    /// Frames the decoder produced as textures.
    pub decoded: u64,
    /// Frames handed to Flutter.
    pub presented: u64,
    /// Frames a newer one replaced before Flutter asked for a texture.
    pub superseded: u64,
    /// From `push` to the frame leaving the pipeline.
    pub decode: Latency,
    /// From `push` to the frame being handed to Flutter's rasteriser.
    pub present: Latency,
}

/// A decoded video stream. Cannot be built on this target.
#[derive(Debug)]
pub struct VideoTexture(std::convert::Infallible);

impl VideoTexture {
    /// Always fails here.
    ///
    /// # Errors
    /// Every call. This target presents through a surface producer.
    pub fn new(_engine_handle: i64) -> Result<Self, Error> {
        Err(Error::WrongTarget)
    }

    /// The identifier a Flutter `Texture` widget draws.
    #[must_use]
    pub fn id(&self) -> i64 {
        match self.0 {}
    }

    /// Hands one encoded access unit to the decoder.
    ///
    /// # Errors
    /// Never reached: no value of this type exists.
    pub fn push(&self, _access_unit: Vec<u8>) -> Result<(), Error> {
        match self.0 {}
    }

    /// Tells the decoder the stream has ended.
    pub fn finish(&self) {
        match self.0 {}
    }

    /// What the path has done so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        match self.0 {}
    }
}
