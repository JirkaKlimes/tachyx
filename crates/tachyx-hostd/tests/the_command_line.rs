//! The command line, against a daemon that is really listening.
//!
//! The binary is run as a person would run it, with `XDG_RUNTIME_DIR` pointing
//! at a socket this test serves. That covers the whole path: the arguments, the
//! framing, the daemon's answer, what is printed and what is exited with.
//!
//! The session side of the daemon is not started, because it is not wired yet.
//! Nothing here needs it.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use tachyx_hostd::config::Config;
use tachyx_hostd::control;
use tachyx_hostd::daemon::Daemon;
use tachyx_proto::control::Codecs;

/// A directory of this test's own, emptied first.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tachyx-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}

/// The real binary, with its control socket under `runtime`.
fn command(runtime: &Path, arguments: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tachyx-hostd"));
    command.args(arguments).env("XDG_RUNTIME_DIR", runtime);
    command
}

/// Runs one, off the test runtime's workers because it blocks.
async fn finish(command: std::process::Command) -> Output {
    tokio::task::spawn_blocking(move || {
        let mut command = command;
        command.output().expect("the binary runs")
    })
    .await
    .expect("the command finishes")
}

/// Runs the real binary against the socket under `runtime`.
async fn run(runtime: &Path, arguments: &[&str]) -> Output {
    finish(command(runtime, arguments)).await
}

/// What one command printed on standard output, as JSON.
fn answered(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {:?}",
            String::from_utf8_lossy(&output.stdout).to_string()
        )
    })
}

/// A daemon holding a socket in `runtime`, and the state it keeps.
async fn daemon(name: &str) -> (Arc<Daemon>, PathBuf) {
    let state = scratch(name);
    let runtime = scratch(&format!("{name}-run"));
    let daemon = Daemon::open(state.join("hostd.toml"), state, Codecs::NONE, false)
        .expect("a daemon opens on an empty directory");
    let listener = control::listen(&runtime.join("tachyx").join("hostd.sock"))
        .await
        .expect("the control socket");
    let serving = Arc::clone(&daemon);
    tokio::spawn(async move {
        let _ = tachyx_hostd::daemon::answer(&serving, listener).await;
    });
    (daemon, runtime)
}

#[tokio::test]
async fn every_command_answers_over_the_socket_a_running_daemon_holds() {
    let (daemon, runtime) = daemon("all").await;

    let identifier = answered(&run(&runtime, &["id", "--json"]).await);
    assert_eq!(identifier["device"], daemon.key.id().to_base32());

    let status = answered(&run(&runtime, &["status", "-j"]).await);
    assert_eq!(status["device"], daemon.key.id().to_base32());
    assert_eq!(status["paired"], 0);
    assert_eq!(status["sessions"].as_array().map(Vec::len), Some(0));

    let pairings = answered(&run(&runtime, &["pairings", "list", "--json"]).await);
    assert_eq!(pairings["pairings"].as_array().map(Vec::len), Some(0));

    let relay = format!(
        "100.64.0.2:47990#{}",
        tachyx_crypto::DeviceKey::from_seed(&[3u8; 32])
            .expect("a key")
            .id()
            .to_base32()
    );
    let added = answered(&run(&runtime, &["relay", "add", &relay, "--json"]).await);
    assert_eq!(added["ok"], true);
    let listed = answered(&run(&runtime, &["relay", "list", "--json"]).await);
    assert_eq!(listed["relays"][0], relay);
    // The daemon read its own file again, so `status` knows about it too.
    let status = answered(&run(&runtime, &["status", "--json"]).await);
    assert_eq!(status["relays"][0]["connected"], false);

    let removed = answered(&run(&runtime, &["relay", "remove", "100.64.0.2:47990", "-j"]).await);
    assert_eq!(removed["ok"], true);
    assert_eq!(
        answered(&run(&runtime, &["relay", "list", "--json"]).await)["relays"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_device_is_narrowed_renamed_and_revoked_by_whatever_names_it() {
    use tachyx_crypto::store::PairedClient;
    use tachyx_crypto::{DeviceKey, Permissions};

    let device = |seed: u8| DeviceKey::from_seed(&[seed; 32]).expect("a key").id();
    let (daemon, runtime) = daemon("devices").await;
    for (seed, name) in [(1u8, "phone"), (2, "laptop")] {
        daemon
            .clients
            .add(PairedClient {
                device: device(seed),
                name: name.to_owned(),
                permissions: Permissions::default(),
                paired: 1_700_000_000 + u64::from(seed),
            })
            .expect("a paired device");
    }

    // By name.
    assert!(
        run(&runtime, &["pairings", "set", "phone", "--allow", "view"])
            .await
            .status
            .success()
    );
    // By a prefix of an identifier, in the case a list prints it in.
    let prefix = device(2).to_string();
    assert!(
        run(&runtime, &["pairings", "rename", &prefix, "desk"])
            .await
            .status
            .success(),
        "the short form a list prints is a prefix, and names one device"
    );

    let listed = answered(&run(&runtime, &["pairings", "list", "--json"]).await);
    let rows = listed["pairings"].as_array().expect("two rows");
    let phone = rows
        .iter()
        .find(|row| row["name"] == "phone")
        .expect("the phone");
    assert_eq!(phone["control"], false, "--allow view narrowed it");
    assert_eq!(
        phone["unattended"], true,
        "and left alone what it did not name"
    );
    assert!(rows.iter().any(|row| row["name"] == "desk"));

    // By the whole identifier.
    assert!(run(
        &runtime,
        &["pairings", "revoke", &device(1).to_base32(), "--json"]
    )
    .await
    .status
    .success());
    let listed = answered(&run(&runtime, &["pairings", "list", "--json"]).await);
    assert_eq!(listed["pairings"].as_array().map(Vec::len), Some(1));

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_command_that_could_not_be_done_says_so_on_standard_error_and_exits_non_zero() {
    let (_daemon, runtime) = daemon("refused").await;

    let output = run(&runtime, &["sessions", "kill", "1", "--json"]).await;
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "nothing may be printed as an answer"
    );
    let complaint: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("one json value on standard error");
    assert_eq!(complaint["error"], "no session 1 is running");

    let output = run(&runtime, &["pairings", "revoke", "phone"]).await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no paired device is called phone"),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_subcommand_with_no_daemon_to_ask_says_the_daemon_is_not_running() {
    let runtime = scratch("absent");
    let output = run(&runtime, &["status"]).await;
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "the daemon is not running"
    );

    let output = run(&runtime, &["status", "--json"]).await;
    let complaint: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("one json value on standard error");
    assert_eq!(complaint["error"], "the daemon is not running");

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn doctor_reports_what_it_found_and_exits_on_whether_anything_failed() {
    let (_daemon, runtime) = daemon("doctor").await;

    let output = run(&runtime, &["doctor", "--json"]).await;
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json value on standard output");
    let checks = report["checks"].as_array().expect("a list of checks");
    assert_eq!(checks[0]["check"], "daemon");
    assert_eq!(checks[0]["state"], "pass");
    // Nothing was probed into this daemon, so it encodes nothing and says so.
    assert!(
        checks
            .iter()
            .any(|check| check["check"] == "encoder" && check["state"] == "fail"),
        "{report}"
    );
    assert!(
        checks
            .iter()
            .any(|check| check["check"] == "pairings" && check["state"] == "warn"),
        "a host with nothing paired accepts nobody, and doctor says so: {report}"
    );
    assert_eq!(report["passed"], false);
    assert!(
        !output.status.success(),
        "a failed check is a non-zero exit"
    );

    let text = run(&runtime, &["doctor"]).await;
    let printed = String::from_utf8_lossy(&text.stdout);
    assert!(printed.contains("ok    the daemon is running"), "{printed}");
    assert!(printed.contains("FAIL  no video encoder"), "{printed}");

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_daemon_that_is_not_running_still_answers_doctor_with_what_it_can_see() {
    // No daemon, and no socket: this is the case the command exists for.
    let runtime = scratch("no-daemon");
    let output = run(&runtime, &["doctor", "--json"]).await;
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json value on standard output");
    let checks = report["checks"].as_array().expect("a list of checks");
    assert_eq!(checks[0]["check"], "daemon");
    assert_eq!(checks[0]["state"], "fail");
    assert!(
        checks.len() > 1,
        "the checks that need no daemon are still worth running: {report}"
    );
    assert!(!output.status.success());

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relays_added_at_the_same_moment_are_all_in_the_list_afterwards() {
    use tachyx_hostd::control::Request;

    let (daemon, runtime) = daemon("concurrent").await;
    let socket = runtime.join("tachyx").join("hostd.sock");
    // One task a connection, and adding a relay is a read of the file, an edit
    // and a write back. Unserialised, every one of these reads the same file
    // and the last write is the only one that survives.
    let adding: Vec<_> = (1..=12u8)
        .map(|seed| {
            let socket = socket.clone();
            tokio::spawn(async move {
                let relay = format!(
                    "10.0.0.{seed}:47990#{}",
                    tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
                        .expect("a key")
                        .id()
                        .to_base32()
                );
                control::ask(
                    &socket,
                    &Request::RelayAdd {
                        relay: relay.parse().expect("a relay"),
                    },
                )
                .await
                .expect("the daemon answers")
            })
        })
        .collect();
    for adding in adding {
        assert_eq!(
            adding.await.expect("a task finishes"),
            tachyx_hostd::control::Response::Done
        );
    }
    assert_eq!(
        Config::load(&daemon.config_path)
            .expect("a file")
            .relays
            .len(),
        12,
        "every add that answered `done` is a relay this host looks for"
    );
    assert_eq!(
        daemon.config().relays.len(),
        12,
        "and the daemon read back what it wrote"
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn an_address_that_would_repaint_a_terminal_is_refused_at_both_ends() {
    use tachyx_hostd::control::{Request, Response};

    let (daemon, runtime) = daemon("address").await;
    let key = tachyx_crypto::DeviceKey::from_seed(&[7u8; 32])
        .expect("a key")
        .id()
        .to_base32();
    let hostile = format!("\u{1b}[41m\u{1b}[2JPWNED\naaa:1#{key}");

    let refused = run(&runtime, &["relay", "add", &hostile]).await;
    assert!(!refused.status.success(), "the command line refuses it");

    // The socket is the boundary that has to hold: the CLI is not the only
    // thing that can send a request over it.
    let answered = control::ask(
        &runtime.join("tachyx").join("hostd.sock"),
        &Request::RelayAdd {
            relay: hostile.parse().expect("a relay"),
        },
    )
    .await
    .expect("the daemon answers");
    assert!(matches!(answered, Response::Failed { .. }), "{answered:?}");
    assert!(
        !daemon
            .config()
            .relays
            .iter()
            .any(|relay| relay.address.contains('\u{1b}')),
        "and nothing that could repaint a terminal was written down"
    );

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_socket_that_cannot_be_reached_at_all_still_gets_a_report() {
    // Longer than the 108 bytes of `sun_path`, so the connection is refused
    // before anything is on the wire. This is the one command whose job is to
    // say why nothing works, and answering with the one error and stopping is
    // answering nothing.
    let runtime = scratch(&format!("far-{}", "a".repeat(120)));
    let output = run(&runtime, &["doctor", "--json"]).await;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {:?}", String::from_utf8_lossy(&output.stderr)));
    let checks = report["checks"].as_array().expect("a list of checks");
    assert_eq!(checks[0]["check"], "daemon");
    assert_eq!(checks[0]["state"], "fail");
    assert!(
        checks[0]["detail"]
            .as_str()
            .expect("a sentence")
            .contains("could not be asked"),
        "the reason the socket refused is folded into the check: {report}"
    );
    assert!(
        checks.len() > 1,
        "the rest of the checks are still run: {report}"
    );
    assert!(!output.status.success());

    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn a_variable_that_names_no_directory_is_not_a_path_relative_to_wherever_this_started() {
    // `var_os` hands back `Some("")` for `XDG_RUNTIME_DIR=`, and joining onto
    // that names `./tachyx/hostd.sock`. This runs in the directory the socket
    // of a live daemon is under, so a build that joins onto the empty variable
    // answers `status` here.
    let (_daemon, runtime) = daemon("empty-runtime").await;
    let mut asked = command(&runtime, &["status", "--json"]);
    asked.env("XDG_RUNTIME_DIR", "").current_dir(&runtime);
    let output = finish(asked).await;
    assert!(
        !output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let complaint: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("one json value on standard error");
    assert_eq!(complaint["error"], "the daemon is not running");

    // The same for the directory this host's private key lives in. There is no
    // answer that is safe to guess, so `doctor` reports it rather than writing
    // a key into whatever directory the daemon was started in. No daemon here:
    // one that is running answers out of the state directory it already found.
    let alone = scratch("no-state");
    let mut asked = command(&alone, &["doctor", "--json"]);
    asked.env("XDG_STATE_HOME", "").env_remove("HOME");
    let output = finish(asked).await;
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json value on standard output");
    let checks = report["checks"].as_array().expect("a list of checks");
    let key = checks
        .iter()
        .find(|check| check["check"] == "device-key")
        .expect("a device-key check");
    assert_eq!(key["state"], "fail");
    assert!(
        key["detail"]
            .as_str()
            .expect("a sentence")
            .contains("XDG_STATE_HOME"),
        "it says which variable would have named it: {report}"
    );

    let _ = std::fs::remove_dir_all(&alone);
    let _ = std::fs::remove_dir_all(&runtime);
}

#[tokio::test]
async fn the_relay_list_a_command_writes_is_a_file_the_daemon_would_start_on() {
    let (daemon, runtime) = daemon("config").await;
    let path = daemon.config_path.clone();
    std::fs::write(&path, "# the laptop\noutput = \"eDP-1\"\n").expect("a configuration");

    let relay = format!(
        "relay.example:47990#{}",
        tachyx_crypto::DeviceKey::from_seed(&[4u8; 32])
            .expect("a key")
            .id()
            .to_base32()
    );
    assert!(
        run(&runtime, &["relay", "add", &relay])
            .await
            .status
            .success(),
        "a relay is added to a file somebody wrote by hand"
    );
    let written = std::fs::read_to_string(&path).expect("the file is still there");
    assert!(written.contains("# the laptop"), "{written}");
    assert_eq!(Config::load(&path).expect("still valid").relays.len(), 1);

    let _ = std::fs::remove_dir_all(&runtime);
}
