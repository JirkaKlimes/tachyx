//! The command line, and what each command does over the daemon's socket.
//!
//! A bare `tachyx-hostd` runs the daemon, because that is what the service unit
//! and every habit already do. Everything else is a thin client: it opens the
//! control socket, asks one question and prints one answer.

use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use tachyx_crypto::{Permissions, RelayAddress};

use crate::config;
use crate::control::{self, ControlError, Pairing, PairingEvent, Request, Response};
use crate::doctor;
use crate::relay::MAILBOX_LIFETIME;
use crate::sessions::SessionId;
use crate::show::{self, Format};

/// The tachyx host daemon, and the commands that drive it.
#[derive(Debug, Parser)]
#[command(name = "tachyx-hostd", version, about, long_about = None)]
pub struct Cli {
    /// What to do. Nothing named runs the daemon.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Print one JSON value instead of text meant for a person.
    #[arg(short = 'j', long, global = true)]
    pub json: bool,
}

/// The daemon, or one command over its socket.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon. This is what a bare `tachyx-hostd` does.
    Run,
    /// Everything else, which needs a daemon to be running.
    #[command(flatten)]
    Client(Client),
}

/// A command over the control socket.
#[derive(Debug, Subcommand)]
pub enum Client {
    /// Print this host's device identifier.
    Id,
    /// Print what the daemon is doing, and every live session.
    Status,
    /// Show a pairing code until a device uses it.
    Pair(PairArgs),
    /// The devices this host has paired with.
    Pairings {
        /// What to do with them.
        #[command(subcommand)]
        command: Pairings,
    },
    /// The sessions running now.
    Sessions {
        /// What to do with them.
        #[command(subcommand)]
        command: Sessions,
    },
    /// The relays this host looks for.
    Relay {
        /// What to do with them.
        #[command(subcommand)]
        command: Relay,
    },
    /// Check everything that has to be true before a session can work.
    Doctor,
}

/// What to do with the paired devices.
#[derive(Debug, Subcommand)]
pub enum Pairings {
    /// List them.
    List,
    /// Forget one, which is the whole of revocation.
    Revoke {
        /// An identifier, an unambiguous prefix of one, or a name.
        device: String,
    },
    /// Change what one may do.
    Set(SetArgs),
    /// Change what one is called in the list.
    Rename {
        /// An identifier, an unambiguous prefix of one, or a name.
        device: String,
        /// What to call it from now on.
        name: String,
    },
}

/// What to do with the live sessions.
#[derive(Debug, Subcommand)]
pub enum Sessions {
    /// End one now.
    Kill {
        /// The number `status` lists it under.
        id: SessionId,
    },
}

/// What to do with the relays.
#[derive(Debug, Subcommand)]
pub enum Relay {
    /// List them, in the order they are tried.
    List,
    /// Add one, replacing any relay already at that address.
    Add {
        /// Where it is and which key it must present: `address:port#KEY`.
        #[arg(value_parser = relay_address)]
        relay: RelayAddress,
    },
    /// Remove every relay at an address.
    Remove {
        /// The address, without a key.
        address: String,
    },
}

/// A pairing on offer.
#[derive(Debug, Clone, Copy, Args)]
pub struct PairArgs {
    /// Grant a picture only. The device cannot type or click.
    #[arg(long)]
    pub view_only: bool,

    /// Require somebody at this machine to accept each session.
    #[arg(long)]
    pub attended: bool,

    /// How long the code stays on offer: `30s`, `5m`, `2h`.
    ///
    /// The default is short of the five minutes a relay keeps a mailbox open,
    /// because an offer that expires with the mailbox it lives in expires a
    /// round trip after it.
    #[arg(long, default_value = "4m", value_parser = ttl)]
    pub ttl: Duration,

    /// Print no QR code, for a terminal that cannot draw one.
    #[arg(long)]
    pub no_qr: bool,
}

impl PairArgs {
    /// What the device that uses this code will be granted.
    #[must_use]
    pub const fn permissions(self) -> Permissions {
        Permissions {
            control: !self.view_only,
            unattended: !self.attended,
        }
    }
}

/// A change to what one device may do.
#[derive(Debug, Args)]
#[command(group = ArgGroup::new("change").required(true).multiple(true).args(["allow", "unattended"]))]
pub struct SetArgs {
    /// An identifier, an unambiguous prefix of one, or a name.
    pub device: String,

    /// Whether it may inject input, or only watch.
    #[arg(long, value_name = "view|control")]
    pub allow: Option<Allow>,

    /// Whether a session opens without somebody here accepting it.
    #[arg(long, value_name = "true|false")]
    pub unattended: Option<bool>,
}

/// What a device is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Allow {
    /// A picture, and nothing else.
    View,
    /// The keyboard and the pointer as well.
    Control,
}

/// Reads the relay of `relay add`.
///
/// The daemon checks the address again before it writes it down. This is here
/// so that a mistyped one is refused by the command that was typed, rather than
/// over a socket by a daemon that may not be running.
fn relay_address(text: &str) -> Result<RelayAddress, String> {
    let relay: RelayAddress = text
        .parse()
        .map_err(|error: tachyx_crypto::CryptoError| error.to_string())?;
    config::check_address(&relay.address).map_err(|error| error.to_string())?;
    Ok(relay)
}

/// The relay commands, done straight against the configuration file.
///
/// What the daemon would have done, and the daemon does exactly this: see
/// `daemon::answer`. A change made here reaches a daemon that starts later
/// because it reads the same file, and one made while a daemon runs goes over
/// the socket instead so that it re-reads at once.
///
/// # Errors
/// If the file cannot be read or written.
fn relay_by_hand(request: &Request) -> Result<Option<Vec<RelayAddress>>, CliError> {
    let path = config::Config::default_path().map_err(|error| CliError::Refused(error.to_string()))?;
    match request {
        Request::RelayList => Ok(Some(config::Config::load(&path).map_err(local)?.relays)),
        Request::RelayAdd { relay } => {
            config::Config::add_relay(&path, relay).map_err(local)?;
            Ok(None)
        }
        Request::RelayRemove { address } => {
            if config::Config::remove_relay(&path, address).map_err(local)? {
                Ok(None)
            } else {
                Err(CliError::Refused(
                    "no relay at that address is configured".to_owned(),
                ))
            }
        }
        _ => Err(CliError::Unexpected),
    }
}

/// A fault from editing the configuration by hand, in the words it came in.
fn local<E: std::fmt::Display>(error: E) -> CliError {
    CliError::Refused(error.to_string())
}

/// Reads `--ttl`.
///
/// Bounded above by [`MAILBOX_LIFETIME`]: a relay closes a mailbox after five
/// minutes, so a longer offer would be a promise this host cannot keep.
fn ttl(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (digits, scale) = match text.as_bytes().last() {
        Some(b's') => (&text[..text.len() - 1], 1),
        Some(b'm') => (&text[..text.len() - 1], 60),
        Some(b'h') => (&text[..text.len() - 1], 3_600),
        _ => (text, 1),
    };
    let seconds: u64 = digits
        .parse()
        .ok()
        .and_then(|count: u64| count.checked_mul(scale))
        .ok_or_else(|| format!("{text} is not a length of time, such as 30s, 5m or 2h"))?;
    if seconds == 0 {
        return Err("a code that is offered for no time at all is not a code".to_owned());
    }
    if seconds > MAILBOX_LIFETIME.as_secs() {
        return Err(format!(
            "a relay closes a pairing mailbox after {} minutes, so {text} is longer than the \
             code could live",
            MAILBOX_LIFETIME.as_secs() / 60
        ));
    }
    Ok(Duration::from_secs(seconds))
}

/// Runs one command over the daemon's socket.
///
/// # Errors
/// If the daemon is not running, the daemon refused, or what was named is not
/// one device.
pub async fn execute(command: Client, format: Format) -> Result<ExitCode, CliError> {
    let socket = control::socket_path();
    match command {
        Client::Id => {
            let Response::Id(device) = ask(&socket, &Request::Id).await? else {
                return Err(CliError::Unexpected);
            };
            print(&show::identifier(device, format))?;
        }
        Client::Status => {
            let Response::Status(status) = ask(&socket, &Request::Status).await? else {
                return Err(CliError::Unexpected);
            };
            print(&show::status(&status, format))?;
        }
        Client::Pair(args) => return pair(args, format).await,
        Client::Pairings { command } => return pairings(command, format).await,
        Client::Sessions {
            command: Sessions::Kill { id },
        } => {
            confirm(&ask(&socket, &Request::SessionsKill { id }).await?)?;
            print(&show::done(format))?;
        }
        Client::Relay { command } => {
            let request = match command {
                Relay::List => Request::RelayList,
                Relay::Add { relay } => Request::RelayAdd { relay },
                Relay::Remove { address } => Request::RelayRemove { address },
            };
            // The configuration file is the truth, and the socket is only how a
            // daemon that is already running is told to read it again. So these
            // work with the daemon down, which is the state a host is in before
            // its first relay is configured: requiring the daemon here would
            // mean a relay could only be added to a daemon that cannot yet be
            // reached from anywhere.
            match control::ask(&socket, &request).await {
                Ok(Response::Relays(relays)) => print(&show::relays(&relays, format))?,
                Ok(answered) => {
                    confirm(&answered)?;
                    print(&show::done(format))?;
                }
                Err(_) => match relay_by_hand(&request)? {
                    Some(relays) => print(&show::relays(&relays, format))?,
                    None => print(&show::done(format))?,
                },
            }
        }
        Client::Doctor => return check(format).await,
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks the daemon, and turns a refusal into an error rather than an answer.
async fn ask(socket: &std::path::Path, request: &Request) -> Result<Response, CliError> {
    match control::ask(socket, request).await? {
        Response::Failed { reason } => Err(CliError::Refused(reason)),
        response => Ok(response),
    }
}

/// Reads an answer that should be "it was done, and there is nothing to say".
fn confirm(response: &Response) -> Result<(), CliError> {
    match response {
        Response::Done => Ok(()),
        _ => Err(CliError::Unexpected),
    }
}

/// Shows a code and reports what happens to it.
async fn pair(args: PairArgs, format: Format) -> Result<ExitCode, CliError> {
    let mut wire = control::connect(&control::socket_path()).await?;
    let ttl = u32::try_from(args.ttl.as_secs()).unwrap_or(u32::MAX);
    wire.send(&Request::Pair {
        permissions: args.permissions(),
        ttl,
    })
    .await?;
    loop {
        let Response::Pairing(event) = wire.recv::<Response>().await? else {
            return Err(CliError::Unexpected);
        };
        match event {
            PairingEvent::Waiting { typed, uri } => {
                // No QR under `--json`: the caller is a program, and the link
                // is in the answer for it to draw one of its own.
                let drawn = (format == Format::Human && !args.no_qr).then(|| show::code(&uri));
                print(&show::waiting(
                    &typed,
                    &uri,
                    u64::from(ttl),
                    drawn.as_deref(),
                    format,
                ))?;
            }
            PairingEvent::Joined => print(&show::joined(format))?,
            PairingEvent::Succeeded {
                device,
                name,
                words,
            } => {
                print(&show::succeeded(device, &name, &words, format))?;
                return Ok(ExitCode::SUCCESS);
            }
            PairingEvent::Failed { reason } => return Err(CliError::Refused(reason)),
        }
    }
}

/// Lists or changes the paired devices.
async fn pairings(command: Pairings, format: Format) -> Result<ExitCode, CliError> {
    let socket = control::socket_path();
    // The list is read first whatever the command is: it is the answer to
    // `list`, and it is what a name or a prefix is resolved against for
    // everything else.
    let Response::Pairings(rows) = ask(&socket, &Request::PairingsList).await? else {
        return Err(CliError::Unexpected);
    };
    let request = match command {
        Pairings::List => {
            print(&show::pairings(&rows, format))?;
            return Ok(ExitCode::SUCCESS);
        }
        Pairings::Revoke { device } => Request::PairingsRevoke {
            device: resolve(&device, &rows)?.device,
        },
        Pairings::Rename { device, name } => Request::PairingsRename {
            device: resolve(&device, &rows)?.device,
            name,
        },
        Pairings::Set(args) => {
            let found = resolve(&args.device, &rows)?;
            Request::PairingsSet {
                device: found.device,
                // What is not named is left as it was, so `--allow view` does
                // not quietly grant unattended access as well.
                permissions: Permissions {
                    control: args
                        .allow
                        .map_or(found.control, |allow| allow == Allow::Control),
                    unattended: args.unattended.unwrap_or(found.unattended),
                },
            }
        }
    };
    confirm(&ask(&socket, &request).await?)?;
    print(&show::done(format))?;
    Ok(ExitCode::SUCCESS)
}

/// Reports why this does not work, with or without a daemon to ask.
async fn check(format: Format) -> Result<ExitCode, CliError> {
    let socket = control::socket_path();
    let report = match control::ask(&socket, &Request::Doctor).await {
        Ok(Response::Doctor(report)) => report,
        Ok(Response::Failed { reason }) => return Err(CliError::Refused(reason)),
        Ok(_) => return Err(CliError::Unexpected),
        // Every way of not reaching the daemon, and not only the daemon not
        // being there. A socket path too long for `sun_path` refuses the same
        // command that would have said so, and the checks that need no daemon
        // are exactly the ones worth running then.
        Err(why) => {
            let asked = socket.clone();
            tokio::task::spawn_blocking(move || doctor::without_daemon(&asked, &why))
                .await
                .map_err(|_| CliError::Unexpected)?
        }
    };
    print(&show::report(&report, format))?;
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Finds the one paired device `selector` names.
///
/// A full identifier, an unambiguous prefix of one, or a name. A prefix that
/// names more than one device is an error that lists them: guessing which
/// device to revoke is not a thing a program may do.
///
/// # Errors
/// If nothing matches, or more than one does.
pub fn resolve(selector: &str, pairings: &[Pairing]) -> Result<Pairing, NoSuchDevice> {
    let wanted = selector.trim();
    if wanted.is_empty() {
        return Err(NoSuchDevice::Unknown(selector.to_owned()));
    }
    let prefix: String = wanted
        .chars()
        .filter(|character| !matches!(character, '-' | ' '))
        .flat_map(char::to_uppercase)
        .collect();
    // A selector of only dashes and spaces leaves nothing, and
    // `starts_with("")` is every device on the list.
    let mut found: Vec<&Pairing> = if prefix.is_empty() {
        Vec::new()
    } else {
        pairings
            .iter()
            .filter(|pairing| pairing.device.to_base32().starts_with(&prefix))
            .collect()
    };
    // Names are looked at only when no identifier matched. A name is chosen by
    // the peer and is never trusted for anything (`spec/pairing.md`), so a
    // device that names itself the identifier of another one must not be able
    // to answer to it.
    if found.is_empty() {
        found = pairings
            .iter()
            .filter(|pairing| pairing.name.eq_ignore_ascii_case(wanted))
            .collect();
    }
    match found.len() {
        0 => Err(NoSuchDevice::Unknown(wanted.to_owned())),
        1 => Ok(found.remove(0).clone()),
        _ => Err(NoSuchDevice::Ambiguous {
            selector: wanted.to_owned(),
            candidates: found
                .iter()
                .map(|pairing| format!("{} ({})", pairing.device, pairing.name))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Writes what a command produced, and flushes it.
///
/// Flushed at every step, because `pair` prints a code and then waits: a code
/// sitting in a buffer is a code nobody can read.
fn print(text: &str) -> Result<(), CliError> {
    use std::io::Write as _;

    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Writes what went wrong, and reports it the way `--json` promised.
pub fn complain(error: &CliError, format: Format) {
    use std::io::Write as _;

    let text = show::failure(&error.to_string(), format);
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// Why a command did not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The daemon could not be reached, or the connection failed.
    #[error(transparent)]
    Control(#[from] ControlError),
    /// The daemon would not, and said why. Its sentence, not this one's.
    #[error("{0}")]
    Refused(String),
    /// What was named on the command line is not one paired device.
    #[error(transparent)]
    Device(#[from] NoSuchDevice),
    /// The answer could not be written.
    #[error("cannot write the answer")]
    Io(#[from] std::io::Error),
    /// The daemon answered something this command has no use for, which is a
    /// fault in this build rather than in anything anybody typed.
    #[error("the daemon answered something else")]
    Unexpected,
}

/// What was named on the command line, and why it names no one device.
#[derive(Debug, thiserror::Error)]
pub enum NoSuchDevice {
    /// Nothing matched.
    #[error("no paired device is called {0}, or has an identifier starting with it")]
    Unknown(String),
    /// More than one matched.
    #[error("{selector} names more than one device: {candidates}")]
    Ambiguous {
        /// What was typed.
        selector: String,
        /// What it could have meant.
        candidates: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    fn parse(line: &str) -> Cli {
        Cli::try_parse_from(line.split_whitespace()).unwrap_or_else(|error| {
            panic!("{line} should parse: {error}");
        })
    }

    fn device(seed: u8) -> tachyx_crypto::DeviceId {
        tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
            .expect("a key")
            .id()
    }

    fn pairing(seed: u8, name: &str) -> Pairing {
        Pairing {
            device: device(seed),
            name: name.to_owned(),
            control: true,
            unattended: true,
            paired: 1_700_000_000,
        }
    }

    #[test]
    fn the_argument_surface_is_the_one_that_was_documented() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_documented_form_of_every_command_parses() {
        for line in [
            "tachyx-hostd",
            "tachyx-hostd run",
            "tachyx-hostd id",
            "tachyx-hostd status",
            "tachyx-hostd pair",
            "tachyx-hostd pair --view-only",
            "tachyx-hostd pair --attended",
            "tachyx-hostd pair --view-only --attended --ttl 5m",
            "tachyx-hostd pair --no-qr",
            "tachyx-hostd pairings list",
            "tachyx-hostd pairings revoke phone",
            "tachyx-hostd pairings set phone --allow view",
            "tachyx-hostd pairings set phone --allow control --unattended true",
            "tachyx-hostd pairings set phone --unattended false",
            "tachyx-hostd pairings rename phone desk-phone",
            "tachyx-hostd sessions kill 3",
            "tachyx-hostd relay list",
            "tachyx-hostd relay remove 100.64.0.2:47990",
            "tachyx-hostd doctor",
        ] {
            let parsed = parse(line);
            assert!(!parsed.json, "{line}");
            let with_json = parse(&format!("{line} --json"));
            assert!(with_json.json, "{line} --json");
            let short = parse(&format!("{line} -j"));
            assert!(short.json, "{line} -j");
        }
    }

    #[test]
    fn a_bare_invocation_and_run_are_the_same_thing() {
        assert!(parse("tachyx-hostd").command.is_none());
        assert!(matches!(
            parse("tachyx-hostd run").command,
            Some(Command::Run)
        ));
    }

    #[test]
    fn a_relay_is_read_with_its_key_or_deliberately_without_one() {
        let written = format!("100.64.0.2:47990#{}", device(1).to_base32());
        let parsed = parse(&format!("tachyx-hostd relay add {written}"));
        let Some(Command::Client(Client::Relay {
            command: Relay::Add { relay },
        })) = parsed.command
        else {
            panic!("a relay add");
        };
        assert_eq!(relay.address, "100.64.0.2:47990");
        assert_eq!(relay.key, Some(device(1)));
        // A bare address is an unpinned relay, which is a decision the
        // operator may make: sessions through it stay end to end, and what is
        // given up is knowing whose relay it is. See spec/relay.md.
        let Some(Command::Client(Client::Relay {
            command: Relay::Add { relay: unpinned },
        })) = Cli::try_parse_from(["tachyx-hostd", "relay", "add", "100.64.0.2:47990"])
            .expect("a bare address parses")
            .command
        else {
            panic!("a relay add");
        };
        assert_eq!(unpinned.key, None);
        assert!(
            Cli::try_parse_from(["tachyx-hostd", "relay", "add", "100.64.0.2:47990#oops"])
                .is_err(),
            "a mistyped key must be refused rather than quietly becoming no pin at all"
        );
    }

    #[test]
    fn a_change_to_what_a_device_may_do_has_to_change_something() {
        assert!(
            Cli::try_parse_from(["tachyx-hostd", "pairings", "set", "phone"]).is_err(),
            "`set` with nothing to set would be a command that silently does nothing"
        );
    }

    #[test]
    fn a_time_to_live_is_read_the_way_a_person_writes_one() {
        assert_eq!(ttl("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(ttl("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(ttl(" 90 ").unwrap(), Duration::from_secs(90));
        assert!(ttl("2h").is_err(), "a mailbox does not live two hours");
        assert!(ttl("0s").is_err());
        assert!(ttl("").is_err());
        assert!(ttl("soon").is_err());
        assert!(ttl("-5m").is_err());
    }

    #[test]
    fn a_device_is_found_by_its_name_or_by_a_prefix_of_its_identifier() {
        let rows = [pairing(1, "phone"), pairing(2, "laptop")];
        assert_eq!(resolve("phone", &rows).unwrap().device, device(1));
        assert_eq!(resolve("PHONE", &rows).unwrap().device, device(1));
        assert_eq!(
            resolve(&device(2).to_base32(), &rows).unwrap().device,
            device(2)
        );
        assert_eq!(
            resolve(&device(2).to_string(), &rows).unwrap().device,
            device(2),
            "the short form a list prints is a thing people will paste back"
        );
        let prefix = &device(2).to_base32()[..6];
        assert_eq!(resolve(prefix, &rows).unwrap().device, device(2));
        assert_eq!(
            resolve(&prefix.to_lowercase(), &rows).unwrap().device,
            device(2),
            "an identifier is read back in either case"
        );
    }

    #[test]
    fn a_prefix_that_names_two_devices_lists_them_rather_than_guessing() {
        let rows = [pairing(1, "phone"), pairing(2, "laptop")];
        let error = resolve("", &rows).expect_err("nothing is not a device");
        assert!(matches!(error, NoSuchDevice::Unknown(_)), "{error}");
        assert!(matches!(
            resolve("desk", &rows).expect_err("no such name"),
            NoSuchDevice::Unknown(_)
        ));

        // Thirty-two identifiers over a thirty-two character alphabet, so two of
        // them start with the same character. That character names both.
        let many: Vec<Pairing> = (1..=32u8)
            .map(|seed| pairing(seed, &format!("device {seed}")))
            .collect();
        let shared = many
            .iter()
            .map(|row| row.device.to_base32()[..1].to_owned())
            .find(|first| {
                many.iter()
                    .filter(|row| row.device.to_base32().starts_with(first))
                    .count()
                    > 1
            })
            .expect("two of thirty-two identifiers share a first character");
        let error = resolve(&shared, &many).expect_err("more than one device matches");
        let NoSuchDevice::Ambiguous { candidates, .. } = &error else {
            panic!("{error}");
        };
        assert!(
            candidates.matches("device ").count() > 1,
            "an ambiguous prefix names what it could have meant: {candidates}"
        );
    }

    #[test]
    fn a_selector_that_filters_down_to_nothing_names_no_device_rather_than_all_of_them() {
        let rows = [pairing(1, "phone")];
        for typed in ["-", " - ", "---", "  "] {
            let error = resolve(typed, &rows)
                .expect_err("a prefix of nothing is a prefix of every identifier");
            assert!(
                matches!(error, NoSuchDevice::Unknown(_)),
                "{typed}: {error}"
            );
        }
    }

    #[test]
    fn a_device_that_names_itself_after_another_one_does_not_answer_for_it() {
        // The name is chosen by the peer. `spec/pairing.md`: it is never
        // trusted for anything, and the identifier is what is compared.
        let liar = pairing(1, &device(2).to_base32());
        let named = pairing(2, "laptop");
        let rows = [liar, named];
        assert_eq!(
            resolve(&device(2).to_base32(), &rows).unwrap().device,
            device(2),
            "an identifier match wins outright, and the name that copied it is not even ambiguous"
        );
        assert_eq!(
            resolve("laptop", &rows).unwrap().device,
            device(2),
            "and a name still finds the device nothing else matched"
        );
    }

    #[test]
    fn what_a_pairing_grants_is_what_the_flags_said() {
        let plain = parse("tachyx-hostd pair");
        let Some(Command::Client(Client::Pair(args))) = plain.command else {
            panic!("a pair");
        };
        assert_eq!(args.permissions(), Permissions::default());
        assert!(
            args.ttl < MAILBOX_LIFETIME,
            "an offer that expires with the mailbox it lives in expires a round trip after it"
        );

        let narrowed = parse("tachyx-hostd pair --view-only --attended");
        let Some(Command::Client(Client::Pair(args))) = narrowed.command else {
            panic!("a pair");
        };
        assert_eq!(
            args.permissions(),
            Permissions {
                control: false,
                unattended: false
            }
        );
    }
}
