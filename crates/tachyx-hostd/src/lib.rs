//! The tachyx host daemon.
//!
//! Capture, encode and input injection live in their own crates. This one is the
//! composition: it owns the session, the pipeline that serves it, and the policy
//! that decides what the encoder is asked for.
//!
//! It is also the command line. `tachyx-hostd` with no subcommand is the
//! daemon; every other subcommand is a thin client over the daemon's control
//! socket, so there is one binary, one configuration and one list of who may
//! reach this host.

pub mod audio;
pub mod cli;
pub mod config;
pub mod control;
pub mod cursor;
pub mod daemon;
pub mod doctor;
pub mod idle;
pub mod idr;
pub mod ladder;
pub mod pairing;
pub mod qr;
pub mod relay;
pub mod session;
pub mod sessions;
pub mod sound;
pub mod show;
