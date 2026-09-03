//! Runs the tachyx relay.
//!
//! The relay prints its own address at startup, in the one written form a person
//! pastes into a configuration file: `addr:port#KEY`. The key is in it because a
//! relay address is configuration somebody pastes once, and pinning on first
//! sight would leave that one moment undefended for no gain.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tachyx_crypto::DeviceKey;
use tachyx_relay::proto::DEFAULT_PORT;
use tachyx_relayd::Relay;

/// Where the state directory goes when nothing says otherwise.
///
/// Under the invoking user rather than under `/var/lib`, because a relay does
/// not need to be root and a static binary somebody dropped on a machine should
/// run without being installed first.
fn default_state() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tachyx-relayd")
}

/// A tachyx relay: presence, rendezvous, reflexive addresses and splicing.
#[derive(Debug, Parser)]
#[command(name = "relayd", version, about)]
struct Arguments {
    /// Where to listen. The relay's port is one above the host's.
    #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)))]
    listen: SocketAddr,
    /// Where this relay's device key lives. Created on first run, mode 0600.
    #[arg(long, default_value_os_t = default_state())]
    state: PathBuf,
    /// Print the startup line as JSON, for a script that reads it.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            // `relayd` as well as `tachyx_relayd`: this file is its own crate,
            // named by the binary, and a filter that left it out would hide the
            // startup line an operator is looking for.
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "relayd=info,tachyx_relayd=info,tachyx_relay=info,tachyx_transport=info".into()
            }),
        )
        // A relay's log is read by whoever runs it, and JSON is for the startup
        // line a script reads rather than for a person watching connections.
        .with_writer(std::io::stderr)
        .init();

    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            let mut source = std::error::Error::source(&*error);
            while let Some(cause) = source {
                tracing::error!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &Arguments) -> Result<(), Box<dyn std::error::Error>> {
    let key = DeviceKey::load_or_create(&arguments.state)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // Bound inside the runtime: an endpoint takes its driver from the
        // ambient runtime when it is created, not when it is first used.
        let relay = Relay::bind(arguments.listen, &key)?;
        announce(&relay, arguments.json)?;
        relay.run().await;
        Ok(())
    })
}

/// Prints the one line an operator needs, in whichever form was asked for.
fn announce(relay: &Relay, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let advertised = relay.advertised()?;
    let bound = relay.local_address()?;
    if json {
        // Escaped by something that knows the rules. The address comes from the
        // command line, and a format string would be safe only until somebody
        // wrote something odd on it.
        println!(
            "{}",
            serde_json::json!({
                "listen": bound.to_string(),
                "relay": advertised.to_string(),
                "device": advertised.key.map(tachyx_crypto::DeviceId::to_base32).unwrap_or_default(),
            })
        );
    } else {
        println!("{advertised}");
    }
    tracing::info!(%bound, relay = %advertised, "tachyx relay listening");
    Ok(())
}
