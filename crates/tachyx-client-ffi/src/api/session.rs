//! Opening a session and feeding it what the user does.

use flutter_rust_bridge::frb;
pub use tachyx_proto::input::InputEvent;
use tachyx_proto::media::Timestamp;

/// Prepares the bridge, before Dart calls anything else.
///
/// Installs the panic hook that turns a Rust panic into a message in the
/// application's log rather than a silent abort on a background thread.
#[frb(init)]
pub fn init() {
    flutter_rust_bridge::setup_default_user_utils();
}

/// The protocol version this build speaks.
///
/// Shown on the connection screen, because a client and a host are updated
/// independently and a version mismatch is the first thing to check.
#[frb(sync)]
#[must_use]
pub fn protocol_version() -> u16 {
    tachyx_proto::VERSION
}

/// What the client's own video path has seen.
///
/// Every field is something this client counted. Nothing here is a figure the
/// host reported about itself, and nothing is an estimate: a statistics overlay
/// built to measure latency is worth less than nothing if it shows a number
/// that was made up.
#[derive(Debug, Clone, Copy)]
pub struct VideoStats {
    /// Datagrams parsed.
    pub packets_received: u64,
    /// Datagrams the sequence number says never arrived.
    pub packets_lost: u64,
    /// Datagrams for a frame that had already been decided.
    pub packets_late: u64,
    /// Frames handed to the decoder.
    pub frames_delivered: u64,
    /// Frames abandoned with fragments missing.
    pub frames_dropped: u64,
    /// Frames that arrived whole and were thrown away anyway, because the
    /// frames they refer back to did not.
    pub frames_undecodable: u64,
}

/// A live session with a host.
///
/// It holds [`tachyx_client::Session`] and adds nothing: the decisions live in
/// that crate, where a test reaches them without a Dart runtime.
#[frb(opaque)]
#[derive(Debug)]
pub struct Session {
    core: tachyx_client::Session,
}

impl Session {
    /// What the video path has seen so far.
    ///
    /// Synchronous, because the overlay reads it once a frame and a round trip
    /// through the isolate to fetch six counters would cost more than the
    /// counters are worth.
    #[frb(sync)]
    #[must_use]
    pub fn video_stats(&self) -> VideoStats {
        let stats = self.core.video_stats();
        VideoStats {
            packets_received: stats.packets_received,
            packets_lost: stats.packets_lost,
            packets_late: stats.packets_late,
            frames_delivered: stats.frames_delivered,
            frames_dropped: stats.frames_dropped,
            frames_undecodable: stats.frames_undecodable,
        }
    }

    /// Records what the user did.
    ///
    /// `at_micros` is when the client saw the event, taken from
    /// `PointerEvent.timeStamp` rather than from the moment this was called, so
    /// that batching does not hide inside the latency measurement.
    pub fn input(&mut self, at_micros: u64, event: InputEvent) {
        self.core
            .input(Timestamp::from_session_micros(at_micros), event);
    }

    /// Ends an input tick and queues everything recorded during it.
    ///
    /// Called on a short timer rather than per event, so that a drag at the
    /// touch sample rate costs one packet instead of one packet per sample.
    ///
    /// # Errors
    /// If the batch does not fit a control message.
    pub fn flush_input(&mut self) -> anyhow::Result<()> {
        self.core.flush_input()?;
        Ok(())
    }
}

/// Opens a session with the host at `address`, for a video surface of this size
/// in physical pixels.
///
/// # Errors
/// Every call, for now, and deliberately. A session needs a QUIC connection to
/// carry its bytes and the crate that dials one is still a placeholder. This
/// says so rather than handing back a session that takes input and drops it: a
/// client that looks connected and does nothing is harder to diagnose than one
/// that names what is missing.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge hands over an owned Dart string; the connection that would consume it is not built yet"
)]
pub fn connect(
    address: String,
    viewport_width: u16,
    viewport_height: u16,
) -> anyhow::Result<Session> {
    Err(anyhow::anyhow!(
        "cannot open a {viewport_width}x{viewport_height} session with {address}: \
         the QUIC transport is not implemented yet"
    ))
}
