//! The same names on a target that has neither present path.
//!
//! Naming the types anyway keeps one shape at the Dart boundary, so a caller
//! gets an error that says what is missing instead of a link that fails.

use tachyx_proto::control::{Codec, Codecs};

use crate::stats::Stats;

/// Why the present path could not be built or run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This target has no decoder this client knows how to reach.
    #[error("this target has no decode and present path")]
    WrongTarget,
}

/// Nothing, because this target decodes nothing.
///
/// An empty set is the honest answer and not a placeholder: a host reading it
/// finds no codec in common and refuses the session, which is what a client
/// with no decoder deserves to be told.
///
/// # Errors
/// Never.
pub fn decoders() -> Result<Codecs, Error> {
    Ok(Codecs::NONE)
}

/// A decoded video stream. Cannot be built on this target.
#[derive(Debug)]
pub struct VideoTexture(std::convert::Infallible);

impl VideoTexture {
    /// Always fails here.
    ///
    /// # Errors
    /// Every call. This target has no decode and present path.
    pub fn new(_handle: i64) -> Result<Self, Error> {
        Err(Error::WrongTarget)
    }

    /// Builds the pipeline for the codec the host chose.
    ///
    /// # Errors
    /// Never reached: no value of this type exists.
    pub fn decode(&self, _codec: Codec) -> Result<(), Error> {
        match self.0 {}
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

    /// The first thing that went wrong, if anything has.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        match self.0 {}
    }

    /// What the path has done so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        match self.0 {}
    }
}
