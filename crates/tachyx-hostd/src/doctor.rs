//! Why does this not work.
//!
//! Every check answers one question a person would otherwise answer by reading
//! a log, and each says what is true rather than naming an error.
//!
//! It runs from either side. When the daemon answers, the report is the
//! daemon's own: it knows what it probed at startup and which relays it has
//! reached. When the daemon does not answer, the CLI runs what it can by itself
//! — the key on disk, the encoders, the compositor — because *the daemon will
//! not start* is exactly when this command is wanted.

use std::path::Path;

use tachyx_proto::control::Codecs;

use crate::audio::{AudioError, Sink};
use crate::config::{Config, PROVISIONAL_RELAY};
use crate::control::{Check, ControlError, RelayStatus, Report};
use crate::daemon::{clients_path, codec_names, Daemon};

/// The name of the file the device key is in, which `tachyx_crypto` chooses.
const KEY_FILE: &str = "device.key";

/// Everything a session needs from the compositor.
///
/// Capture, injection, and idle inhibition. The last one is in the list because
/// a session takes an inhibitor before it starts: a blanked screen produces no
/// frames, and the one person who wants the machine awake is the one person who
/// cannot wake it.
const COMPOSITOR: [&str; 7] = [
    "ext_output_image_capture_source_manager_v1",
    "ext_image_copy_capture_manager_v1",
    "zwp_linux_dmabuf_v1",
    "zwlr_virtual_pointer_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zxdg_output_manager_v1",
    "zwp_idle_inhibit_manager_v1",
];

/// What the daemon reports about itself.
///
/// Blocking: it talks to the compositor. Run it off the runtime's workers.
#[must_use]
pub fn of_daemon(daemon: &Daemon) -> Report {
    let config = daemon.config();
    let mut checks = vec![Check::new(
        "daemon",
        true,
        "the daemon is running and its socket is reachable",
    )];
    checks.push(device_key(&daemon.state));
    checks.push(encoders(daemon.encoders));
    checks.push(audio(&crate::audio::probe(), Some(daemon.audio)));
    checks.push(compositor());
    checks.extend(relays(&daemon.presence.snapshot(), &config));
    checks.push(pairings(daemon.clients.list().len()));
    Report { checks }
}

/// What the CLI can find out with no daemon to ask.
///
/// `why` is whatever the socket refused with, and it is folded into the first
/// check rather than reported instead of everything else. This is the one
/// command whose job is to say why nothing works: a command that answers *the
/// socket path is too long* and stops has answered nothing.
///
/// It reads the configuration and finds the state directory itself, because
/// either of those failing is a thing to report as a check rather than a
/// reason not to run the rest.
///
/// Blocking: it talks to the compositor. Run it off the runtime's workers.
#[must_use]
pub fn without_daemon(socket: &Path, why: &ControlError) -> Report {
    let mut checks = vec![unreachable(socket, why)];
    let config = match Config::default_path().and_then(|path| Config::load(&path)) {
        Ok(config) => config,
        Err(error) => {
            checks.push(Check::new(
                "config",
                false,
                format!("{error}: the daemon will not start until this is fixed"),
            ));
            Config::default()
        }
    };
    let state = Config::state_directory();
    checks.push(match &state {
        Ok(state) => device_key(state),
        Err(error) => Check::new("device-key", false, error.to_string()),
    });
    checks.push(match tachyx_media::encoders() {
        Ok(found) => encoders(found),
        Err(error) => Check::new(
            "encoder",
            false,
            format!("the gstreamer registry could not be read: {error}"),
        ),
    });
    checks.push(audio(&crate::audio::probe(), None));
    checks.push(compositor());
    checks.extend(relays(&[], &config));
    checks.push(match state {
        Ok(state) => match tachyx_crypto::PairedClients::open(clients_path(&state)) {
            Ok(clients) => pairings(clients.list().len()),
            Err(error) => Check::new("pairings", false, error.to_string()),
        },
        Err(error) => Check::new("pairings", false, error.to_string()),
    });
    Report { checks }
}

/// Why the daemon could not be asked, which is the first thing a person needs.
fn unreachable(socket: &Path, why: &ControlError) -> Check {
    Check::new(
        "daemon",
        false,
        match why {
            ControlError::NotRunning => format!(
                "the daemon is not running: nothing is listening on {}. start it with \
                 `tachyx-hostd`, and read what it says as it refuses",
                socket.display()
            ),
            other => format!(
                "the daemon could not be asked over {}: {}",
                socket.display(),
                causes(other)
            ),
        },
    )
}

/// An error and everything under it, in one sentence.
///
/// The half a person needs is in the source: the top of a socket error says
/// *cannot use the control socket at ...* and the one underneath says *File
/// name too long*.
fn causes(error: &dyn std::error::Error) -> String {
    use std::fmt::Write as _;

    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}

/// The seed on disk, and who can read it.
fn device_key(state: &Path) -> Check {
    let path = state.join(KEY_FILE);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Check::new(
            "device-key",
            false,
            format!(
                "there is no {}: the daemon writes one at its first start, so it has never \
                 started here",
                path.display()
            ),
        );
    };
    let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777;
    Check::new(
        "device-key",
        mode == 0o600,
        if mode == 0o600 {
            format!(
                "this host's key is in {} and nobody else can read it",
                path.display()
            )
        } else {
            format!(
                "this host's key in {} is mode {mode:04o}: anybody who can read it is this \
                 host. `chmod 600` it",
                path.display()
            )
        },
    )
}

/// What the encoders were probed to offer.
fn encoders(found: Codecs) -> Check {
    let names = codec_names(found);
    Check::new(
        "encoder",
        !names.is_empty(),
        if names.is_empty() {
            "no video encoder: the gstreamer registry offers none of vah264enc, vah264lpenc, \
             vaav1enc or vaav1lpenc. the plugins are missing from this environment (run inside \
             `nix develop`), or this machine has no va-api"
                .to_owned()
        } else {
            format!("this host encodes {}", names.join(", "))
        },
    )
}

/// What this host can do with its own sound.
///
/// A warning rather than a failure when it can do nothing. Sound is optional:
/// a session without it is still a session, and the hello tells the client not
/// to offer a control for it.
///
/// `announced` is what the daemon probed when it started, which is what every
/// client has been told since. It is worth saying out loud when it no longer
/// matches, because a sound server started after the daemon is a host that has
/// sound and offers none until it is restarted.
fn audio(found: &Result<Sink, AudioError>, announced: Option<bool>) -> Check {
    match found {
        Ok(sink) if announced == Some(false) => Check::warn(
            "audio",
            format!(
                "this host has sound at {} now and announced none when it started. restart the \
                 daemon to offer it",
                sink.monitor()
            ),
        ),
        Ok(sink) => Check::new(
            "audio",
            true,
            format!("this host streams its own sound from {}", sink.monitor()),
        ),
        Err(error) => Check::warn("audio", format!("{error}. sessions run without sound")),
    }
}

/// What the compositor offers.
fn compositor() -> Check {
    let offered = match globals() {
        Ok(offered) => offered,
        Err(error) => {
            return Check::new(
                "compositor",
                false,
                format!(
                    "cannot reach the wayland compositor: {error}. the daemon has to run inside \
                     the graphical session it is capturing"
                ),
            );
        }
    };
    let missing: Vec<&str> = COMPOSITOR
        .into_iter()
        .filter(|wanted| !offered.iter().any(|offered| offered == wanted))
        .collect();
    Check::new(
        "compositor",
        missing.is_empty(),
        if missing.is_empty() {
            "the compositor offers capture, input injection and idle inhibition".to_owned()
        } else {
            format!("the compositor does not offer {}", missing.join(", "))
        },
    )
}

/// One row per configured relay, and one row saying so when there are none.
fn relays(known: &[RelayStatus], config: &Config) -> Vec<Check> {
    if config.relays.is_empty() {
        return vec![Check::warn(
            "relay",
            format!(
                "no relay is configured, so this host can be reached only from this network and \
                 can pair with nobody. a fresh host would have used {PROVISIONAL_RELAY}; \
                 `tachyx-hostd relay add` puts one back"
            ),
        )];
    }
    config
        .relays
        .iter()
        .map(|relay| {
            let held = known.iter().find(|held| held.relay == *relay);
            match held {
                Some(held) if held.connected => Check::new(
                    "relay",
                    true,
                    match held.rtt {
                        Some(rtt) => format!(
                            "the relay {} answered{}, {:.1} ms away",
                            relay.address,
                            pinned(relay),
                            f64::from(rtt) / 1000.0
                        ),
                        None => {
                            format!("the relay {} answered{}", relay.address, pinned(relay))
                        }
                    },
                ),
                Some(_) => Check::new(
                    "relay",
                    false,
                    format!(
                        "no answer from the relay {}, or it presented another key",
                        relay.address
                    ),
                ),
                None => Check::warn(
                    "relay",
                    format!(
                        "nothing has tried the relay {} yet, because the daemon is not running",
                        relay.address
                    ),
                ),
            }
        })
        .collect()
}

/// How many devices are paired, and the warning when none are.
fn pairings(count: usize) -> Check {
    if count == 0 {
        return Check::warn(
            "pairings",
            "no device is paired, so this host accepts nobody. run `tachyx-hostd pair` and use \
             the code on the device you want to let in",
        );
    }
    Check::new(
        "pairings",
        true,
        format!("{count} device{} may reach this host", plural(count)),
    )
}

/// The `s` on a count that is not one.
const fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Why the compositor could not be asked what it offers.
#[derive(Debug, thiserror::Error)]
enum WaylandError {
    /// There is no compositor to ask, or this process is not in its session.
    #[error("cannot connect to the wayland compositor")]
    Connect(#[from] wayland_client::ConnectError),
    /// The connection failed while it was being asked.
    #[error("the wayland connection failed")]
    Dispatch(#[from] wayland_client::DispatchError),
}

/// Every interface the compositor announces.
///
/// How a relay's identity was checked, for the sentence that reports it.
///
/// An unpinned relay is a choice the operator made, and the report keeps saying
/// what it costs rather than saying it once at `relay add` and never again.
fn pinned(relay: &tachyx_crypto::RelayAddress) -> &'static str {
    if relay.key.is_some() {
        " with the key it is pinned to"
    } else {
        ". it is not pinned, so anyone who can intercept that address could be it"
    }
}

/// The names only. Nothing is bound, so this cannot take a virtual pointer away
/// from the daemon that is using one, and it costs one round trip.
fn globals() -> Result<Vec<String>, WaylandError> {
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle};

    #[derive(Debug, Default)]
    struct Globals {
        names: Vec<String>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event {
                state.names.push(interface);
            }
        }
    }

    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let mut globals = Globals::default();
    queue.roundtrip(&mut globals)?;
    Ok(globals.names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::State;

    #[test]
    fn a_host_with_nothing_paired_is_warned_about_rather_than_failed() {
        assert_eq!(pairings(0).state, State::Warn);
        assert_eq!(pairings(1).state, State::Pass);
        assert!(pairings(1).detail.contains("1 device may"));
        assert!(pairings(2).detail.contains("2 devices may"));
    }

    #[test]
    fn a_key_anybody_can_read_is_a_failure_that_says_what_to_do() {
        use std::os::unix::fs::PermissionsExt as _;

        let state = std::env::temp_dir().join(format!("tachyx-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        assert_eq!(device_key(&state).state, State::Fail, "no key at all");

        std::fs::create_dir_all(&state).unwrap();
        let path = state.join(KEY_FILE);
        std::fs::write(&path, [0u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let check = device_key(&state);
        assert_eq!(check.state, State::Fail);
        assert!(check.detail.contains("chmod 600"), "{}", check.detail);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(device_key(&state).state, State::Pass);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_host_whose_relay_list_is_empty_is_told_what_that_costs() {
        let checks = relays(
            &[],
            &Config {
                relays: Vec::new(),
                ..Config::default()
            },
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].state, State::Warn);
        assert!(
            checks[0].detail.contains(PROVISIONAL_RELAY),
            "it names the one a host would otherwise have had"
        );
    }

    #[test]
    fn a_relay_nothing_has_reached_is_a_failure_when_the_daemon_is_running() {
        let relay: tachyx_crypto::RelayAddress = format!(
            "100.64.0.2:47990#{}",
            tachyx_crypto::DeviceKey::from_seed(&[1u8; 32])
                .unwrap()
                .id()
                .to_base32()
        )
        .parse()
        .unwrap();
        let config = Config {
            relays: vec![relay.clone()],
            ..Config::default()
        };
        let known = [RelayStatus {
            relay: relay.clone(),
            connected: false,
            rtt: None,
        }];
        assert_eq!(relays(&known, &config)[0].state, State::Fail);

        let reached = [RelayStatus {
            relay,
            connected: true,
            rtt: Some(12_400),
        }];
        let check = &relays(&reached, &config)[0];
        assert_eq!(check.state, State::Pass);
        assert!(check.detail.contains("12.4 ms"), "{}", check.detail);
    }

    #[test]
    fn a_report_with_a_warning_and_no_failure_has_passed() {
        let report = Report {
            checks: vec![
                Check::new("daemon", true, "up"),
                Check::warn("pairings", "nobody"),
            ],
        };
        assert!(report.passed());
        let failed = Report {
            checks: vec![Check::new("encoder", false, "none")],
        };
        assert!(!failed.passed());
    }
}
