//! Runs the tachyx host daemon.

use std::process::ExitCode;

use tachyx_hostd::config::Config;
use tachyx_hostd::session;
use tachyx_proto::control::CodedSize;
use tachyx_transport::TransportError;
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

    // Sized before any client says what it wants, and deliberately not for the
    // largest picture imaginable. The send gate's bound IS this buffer, so a
    // generous one does not go unused: it is filled, and standing queue is
    // latency. At 3840x2160 it came to 809 kB, which is two thirds of a second
    // of video waiting to be sent, read by the congestion controller as
    // bandwidth and paid for by every heartbeat queued behind it.
    //
    // 1080p is the largest a phone asks for, and a client that wants more is
    // better served by a buffer that keeps its picture recent.
    let ceiling = CodedSize::new(1920, 1088).ok_or("1920x1088 is not a valid coded size")?;

    // Injection owns a Wayland connection and a thread of its own, so that a
    // stall in it holds up neither capture nor the session it serves. Started
    // once and shared by every session: reaching the compositor costs several
    // round trips, and the keyboard modifier state it holds is the state of a
    // real keyboard, which does not belong to any one client.
    let injection = tachyx_input::Injection::spawn(config.output.as_deref().unwrap_or("eDP-1"))?;

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
            // A connection that fails to establish is that client's problem, not
            // the daemon's. Only losing the socket itself ends the loop.
            let session = match host.accept().await {
                Ok(session) => session,
                Err(TransportError::Closed) => return Err(TransportError::Closed.into()),
                Err(error) => {
                    tracing::warn!(%error, "a client failed to connect");
                    continue;
                }
            };
            let peer = session.connection.remote_address();
            tracing::info!(%peer, "client connected");
            match session::serve(&config, session, &injection).await {
                Ok(()) => tracing::info!(%peer, "client disconnected"),
                Err(error) => tracing::warn!(%peer, %error, "session ended"),
            }
        }
    })
}
