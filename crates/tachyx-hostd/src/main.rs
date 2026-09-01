//! Runs the tachyx host daemon.

use std::process::ExitCode;

use tachyx_hostd::config::Config;
use tachyx_hostd::session;
use tachyx_proto::control::CodedSize;
use tachyx_transport::endpoint::{Host, Tuning};
use tachyx_transport::identity::HostIdentity;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "tachyx_hostd=info,tachyx_media=info,tachyx_transport=info".into()
            }),
        )
        .init();

    match run() {
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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(&Config::default_path())?;
    let state = Config::state_directory();
    std::fs::create_dir_all(&state)?;

    let identity = HostIdentity::load_or_create(&state)?;

    // Sized before any client says what it wants, so it is sized for the largest
    // picture this host could ever be asked for. Over-sizing costs nothing: the
    // send gate bounds what is actually queued, while a buffer too small to hold
    // one key frame would shred every key frame rather than merely slow the
    // stream down.
    let ceiling = CodedSize::new(3840, 2160).ok_or("3840x2160 is not a valid coded size")?;

    // Injection owns a Wayland connection of its own, so that a stall in it
    // cannot hold up capture.
    let mut injector = tachyx_input::Injector::new(config.output.as_deref().unwrap_or("eDP-1"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // Bound inside the runtime: the endpoint takes its driver from the
        // ambient runtime when it is created, not when it is first used.
        let host = Host::bind(config.listen, &identity, Tuning::for_stream(ceiling))?;
        tracing::info!(
            address = %host.local_address()?,
            fingerprint = %identity.fingerprint().to_hex(),
            "tachyx-hostd listening"
        );
        tracing::warn!(
            "no authentication: anything that can reach this port can watch and control this machine"
        );

        loop {
            let session = host.accept().await?;
            let peer = session.connection.remote_address();
            tracing::info!(%peer, "client connected");
            match session::serve(&config, session, &mut injector).await {
                Ok(()) => tracing::info!(%peer, "client disconnected"),
                Err(error) => tracing::warn!(%peer, %error, "session ended"),
            }
        }
    })
}
