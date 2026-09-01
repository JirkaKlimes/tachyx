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
