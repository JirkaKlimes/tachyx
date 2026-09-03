//! Runs the tachyx host daemon, or one command over the socket of a daemon
//! that is already running.

use std::process::ExitCode;

use clap::Parser as _;
use tachyx_hostd::cli::{self, Cli, Command};
use tachyx_hostd::daemon;
use tachyx_hostd::show::Format;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = Format::of(cli.json);
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(),
        Command::Client(command) => client(command, format),
    }
}

/// The daemon.
///
/// Its own log goes through `tracing`, which a subcommand does not start:
/// anything a subcommand writes to standard output is its answer, and a log
/// line in the middle of it would be a second thing for a script to read.
fn run() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "tachyx_hostd=info,tachyx_media=info,tachyx_transport=info".into()
            }),
        )
        .init();

    match daemon::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                tracing::error!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

/// One command over the daemon's control socket.
fn client(command: cli::Client, format: Format) -> ExitCode {
    // One thread. A subcommand opens one socket, writes one message and waits,
    // and a worker pool for that would be a pool that starts and stops for
    // nothing.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(cli::execute(command, format)) {
        Ok(code) => code,
        Err(error) => {
            cli::complain(&error, format);
            ExitCode::FAILURE
        }
    }
}
