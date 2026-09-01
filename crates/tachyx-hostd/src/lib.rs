//! The tachyx host daemon.
//!
//! Capture, encode and input injection live in their own crates. This one is the
//! composition: it owns the session, the pipeline that serves it, and the policy
//! that decides what the encoder is asked for.

pub mod idle;
pub mod idr;
pub mod ladder;
