/// What the play path has counted.
///
/// Every figure is something this crate did. Nothing here is the host's word for
/// anything, and nothing is an estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets decoded and played.
    pub played: u64,
    /// Frames of sound the decoder invented.
    ///
    /// Two causes, and a listener cannot tell them apart: a hole nothing carried
    /// a copy of, and the buffer gathering sound again after it ran dry.
    /// [`Stats::underruns`] is what separates them.
    pub concealed: u64,
    /// Frames rebuilt out of the packet behind the hole.
    ///
    /// The host encodes with forward error correction, so a packet carries a
    /// coarse copy of the one before it. A frame counted here is a frame the
    /// listener heard rather than a gap the decoder smoothed over.
    pub recovered: u64,
    /// Frames thrown away to hold the buffer down.
    ///
    /// The host's sound card and this machine's run at slightly different rates,
    /// so one of them is always slowly gaining on the other. This is that
    /// correction in the direction where sound piles up.
    pub dropped: u64,
    /// Times the buffer ran dry with the device still asking for sound.
    pub underruns: u64,
    /// Frames waiting to be played right now.
    pub buffered: u32,
}
