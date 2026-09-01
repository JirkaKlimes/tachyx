//! Everything Dart can call.
//!
//! Nothing here decides anything. It names the protocol's types so that
//! `flutter_rust_bridge` can generate their Dart twins, and forwards calls to
//! `tachyx-client`. A rule that lives here is a rule the tests in that crate
//! cannot reach.

pub mod input;
pub mod session;
pub mod video;
