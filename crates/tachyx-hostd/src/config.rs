//! What the daemon reads at startup.
//!
//! One TOML file, in the usual place, every field optional. A host with no
//! configuration at all serves its first output on the default port, which is
//! what makes the daemon worth starting before anyone has read any of this.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tachyx_crypto::RelayAddress;

/// The port the daemon listens on when nothing says otherwise.
///
/// The client's own default must match this, in
/// `app/lib/src/session/link.dart`.
pub const DEFAULT_PORT: u16 = 47_989;

/// The most characters a relay address may be.
///
/// The address travels inside the pairing link, and the link has to stay inside
/// the 41 modules a terminal can draw a QR code in. Measured: the link crosses
/// into 45 modules at an address of 49 characters, which is 53 columns of
/// half-blocks and off the side of an 80-column terminal. This is that ceiling
/// with room left over, and `qr.rs` asserts an address of exactly this length
/// still fits.
pub const ADDRESS_LIMIT: usize = 40;

/// The relay this project will run first, and the only place its address is
/// written.
///
/// **Provisional, and it is a tailnet address**, so nothing off that tailnet can
/// reach it. A public one replaces both halves when a relay is deployed
/// somewhere that stays up, and the key changes with it because the key is that
/// relay's identity.
///
/// The default relay list holds this one and nothing else. `tachyx-hostd relay
/// add <address:port#KEY>` is how another is set, and `relay remove` is how this
/// one goes. Both work with the daemon down, because this file is the truth and
/// the control socket is only how a running daemon is told to read it again.
pub const PROVISIONAL_RELAY: &str = "100.64.0.2:47990#27JOBTX5HG5UTF5ELHXL6YLX26EKX3PH6O4S7V4IT3VGMV37LV5Q";

/// Settings for the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Address to listen on.
    ///
    /// The default accepts from anywhere, because a host nobody can reach from
    /// their phone is not serving its purpose. There is no authentication yet,
    /// so this belongs on a network you trust and nowhere else.
    pub listen: SocketAddr,

    /// Which monitor to capture, by the name the compositor gives it.
    ///
    /// Unset means the first output the compositor reports.
    pub output: Option<String>,

    /// Highest bitrate to ask the encoder for, in bits per second.
    ///
    /// The rate controller works below this. It exists so that a fast link does
    /// not spend more than the user wants on a picture they cannot tell apart.
    pub max_bitrate: u32,

    /// Frame rate ceiling.
    ///
    /// Unset follows the monitor's own refresh rate, which is what the encoder's
    /// bit budget is computed against.
    pub max_framerate: Option<u32>,

    /// Where this host looks for a home relay, tried in order.
    ///
    /// Each is an address and the key that relay must present. The default is
    /// [`PROVISIONAL_RELAY`] alone. An empty list is a decision rather than a
    /// default, and it is not a working state: pairing runs over a relay
    /// mailbox, so a host with no relay is reachable only from its own network
    /// and can pair with nobody.
    ///
    /// Written by `tachyx-hostd relay add`, which edits this file in place.
    pub relays: Vec<RelayAddress>,

    /// What this host calls itself in a pairing.
    ///
    /// It is a label for a person reading a list on the other device and it
    /// decides nothing: the identifier is what is compared. Unset is this
    /// machine's hostname.
    pub name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT),
            output: None,
            max_bitrate: 40_000_000,
            max_framerate: None,
            // The provisional relay, so that a host with no configuration
            // file is still reachable and can still pair. Parsing it cannot
            // fail: it is a constant this crate's own test reads.
            relays: PROVISIONAL_RELAY
                .parse()
                .map(|relay| vec![relay])
                .unwrap_or_default(),
            name: None,
        }
    }
}

/// Why the configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {path}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid configuration.
    #[error("{path} is not valid configuration")]
    Parse {
        /// The file that could not be parsed.
        path: PathBuf,
        /// Why.
        #[source]
        source: toml::de::Error,
    },
    /// The file could not be written back.
    #[error("cannot write {path}")]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The file could not be edited in place: it is not TOML, its `relays` key
    /// is not a list, or the edit would leave a file the daemon refuses to
    /// start on.
    #[error("cannot edit {path}: {detail}")]
    Edit {
        /// The file.
        path: PathBuf,
        /// What is in the way.
        detail: String,
    },
    /// Nothing names the directory this daemon keeps something in.
    ///
    /// The directory it happened to be started in is not an answer: the state
    /// directory holds a private key at mode 0600, and the configuration
    /// directory decides which relays this host trusts.
    #[error(
        "neither {variable} nor HOME names an absolute directory, so there is nowhere to keep \
         the {what}"
    )]
    NoDirectory {
        /// The variable that would have named it.
        variable: &'static str,
        /// What has nowhere to live.
        what: &'static str,
    },
    /// A relay address that is not one.
    ///
    /// It does not quote what was offered. The address is printed back on the
    /// terminal that complains, and quoting it there would be the escape
    /// sequence this refused arriving by another road.
    #[error(
        "a relay address is a host and a port, written with letters, digits and `.-_:[]`, and \
         no more than {ADDRESS_LIMIT} characters"
    )]
    Address,
}

/// Refuses a relay address that is not one.
///
/// The address is written into a file the daemon parses at every start, and it
/// is printed by `relay list` and by `status`. `relay add
/// $'\e[41m\e[2JPWNED\naaa:1#<KEY>'` is otherwise a relay list that repaints
/// the screen of whoever runs it.
///
/// # Errors
/// If it is empty, longer than [`ADDRESS_LIMIT`], or holds anything but the
/// characters a host and a port are written with.
pub fn check_address(address: &str) -> Result<(), ConfigError> {
    let shaped = !address.is_empty()
        && address.len() <= ADDRESS_LIMIT
        && address.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '-' | '_' | ':' | '[' | ']')
        });
    if shaped {
        Ok(())
    } else {
        Err(ConfigError::Address)
    }
}

impl Config {
    /// Reads the configuration, falling back to the defaults when there is none.
    ///
    /// A missing file is not an error. A malformed one is: silently ignoring a
    /// file the user wrote would be worse than refusing to start.
    ///
    /// # Errors
    /// If the file exists and cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    /// Where the configuration lives when no path is given.
    ///
    /// # Errors
    /// If neither `XDG_CONFIG_HOME` nor `HOME` names an absolute directory.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        Ok(under_home("XDG_CONFIG_HOME", ".config", "configuration")?
            .join("tachyx")
            .join("hostd.toml"))
    }

    /// What this host calls itself in a pairing.
    #[must_use]
    pub fn host_name(&self) -> String {
        self.name.clone().unwrap_or_else(hostname)
    }

    /// Adds `relay` to the list in the file at `path`, replacing any relay at
    /// the same address.
    ///
    /// The file is edited rather than rewritten, so the comments, the spacing
    /// and the key order somebody typed survive. The result is parsed back
    /// before it is written: an edit that leaves a file the daemon would refuse
    /// to start on is worse than no edit.
    ///
    /// # Errors
    /// If the address is not one, or the file cannot be read, parsed, or
    /// written.
    pub fn add_relay(path: &Path, relay: &RelayAddress) -> Result<(), ConfigError> {
        // Checked here rather than only in the CLI: this is where the address
        // becomes a line in a file that other commands print.
        check_address(&relay.address)?;
        edit_relays(path, |relays| {
            relays.retain(|held| !is_at(held, &relay.address));
            relays.push(relay.to_string());
            true
        })
    }

    /// Removes every relay at `address`, reporting whether there was one.
    ///
    /// # Errors
    /// If the file cannot be read, parsed, or written.
    pub fn remove_relay(path: &Path, address: &str) -> Result<bool, ConfigError> {
        let mut removed = false;
        edit_relays(path, |relays| {
            let before = relays.len();
            relays.retain(|held| !is_at(held, address));
            removed = relays.len() != before;
            removed
        })?;
        Ok(removed)
    }

    /// Where the host's own key and its record of paired clients live.
    ///
    /// # Errors
    /// If neither `XDG_STATE_HOME` nor `HOME` names an absolute directory. A
    /// daemon that cannot find this says so and refuses to start, rather than
    /// writing a 0600 private key into whatever directory it was started in.
    pub fn state_directory() -> Result<PathBuf, ConfigError> {
        Ok(under_home("XDG_STATE_HOME", ".local/state", "device key")?.join("tachyx"))
    }
}

/// The directory `variable` names, when it names one at all.
///
/// `var_os` hands back `Some("")` for a variable that is set and empty, and a
/// relative path joined onto is a path under the directory the daemon happened
/// to be started in. Neither is a directory anything here may use, so both read
/// as unset.
fn directory(variable: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(variable)?);
    path.is_absolute().then_some(path)
}

/// The directory `variable` names, or `under` beneath the home directory.
fn under_home(
    variable: &'static str,
    under: &str,
    what: &'static str,
) -> Result<PathBuf, ConfigError> {
    directory(variable)
        .or_else(|| directory("HOME").map(|home| home.join(under)))
        .ok_or(ConfigError::NoDirectory { variable, what })
}

/// The runtime directory, when this session has one.
pub(crate) fn runtime_directory() -> Option<PathBuf> {
    directory("XDG_RUNTIME_DIR")
}

/// Whether a written relay names `address`, whatever key follows it.
///
/// A relay that rotated its key is the same relay, and adding it again must
/// replace the old row rather than leave two rows the host would try in turn.
fn is_at(written: &toml_edit::Value, address: &str) -> bool {
    written
        .as_str()
        .and_then(|text| text.trim().rsplit_once('#'))
        .is_some_and(|(held, _)| held == address)
}

/// Applies `change` to the `relays` list in the file at `path`, and writes the
/// file when it changed something.
fn edit_relays(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::Array) -> bool,
) -> Result<(), ConfigError> {
    let refused = |detail: &str| ConfigError::Edit {
        path: path.to_owned(),
        detail: detail.to_owned(),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| refused(&error.to_string()))?;
    let item = document
        .entry("relays")
        .or_insert(toml_edit::Item::Value(toml_edit::Value::Array(
            toml_edit::Array::new(),
        )));
    let Some(relays) = item.as_array_mut() else {
        return Err(refused("the relays key is not a list"));
    };
    if !change(relays) {
        return Ok(());
    }
    let edited = document.to_string();
    // Read back before it is written. An edit that leaves a file the daemon
    // refuses to start on is worse than no edit, and the parse here is the only
    // thing standing between a `relay add` and a host that will not come up.
    toml::from_str::<Config>(&edited).map_err(|error| refused(&error.to_string()))?;
    write_whole(path, &edited)
}

/// Writes a file whole, or leaves the old one.
///
/// A temporary beside the target and a rename over it. The configuration is
/// read at every start, and a crash part way through a write would leave a file
/// the daemon refuses.
///
/// Three things here are not decoration, and `tachyx_crypto::store` does the
/// same three for the same reasons.
///
/// The temporary is named at random and opened with `create_new`. A name every
/// writer picks is a name two of them share, and this process runs one writer a
/// control connection: the second would open the first one's temporary and
/// truncate it. A predictable name is also a name somebody can plant a file or
/// a symlink at, and `create_new` refuses both rather than writing through
/// them.
///
/// The mode of the file being replaced is carried over, because the new file is
/// the old one: a configuration somebody chmodded 0600 must not come back 0644.
///
/// The contents are flushed before the rename and the directory after it. A
/// rename can otherwise reach the disk while the bytes do not, and a truncated
/// configuration is a daemon that refuses to start.
fn write_whole(path: &Path, text: &str) -> Result<(), ConfigError> {
    use std::hash::{BuildHasher as _, RandomState};
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let failed = |source| ConfigError::Write {
        path: path.to_owned(),
        source,
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(failed)?;
    let mode = std::fs::metadata(path).map_or(0o600, |held| held.permissions().mode() & 0o777);
    let temporary = path.with_extension(format!("tmp{:016x}", RandomState::new().hash_one(path)));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if written.is_err() {
        // A full disk otherwise fills it further at every attempt.
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(failed)
}

/// The kernel's idea of this machine's name.
///
/// Read out of `/proc` rather than through `uname`, because the daemon captures
/// through Wayland and encodes through VA-API and so is Linux only anyway.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "tachyx host".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_yields_the_defaults() {
        let missing = Path::new("/nonexistent/tachyx/hostd.toml");
        assert_eq!(Config::load(missing).unwrap(), Config::default());
    }

    #[test]
    fn a_malformed_file_refuses_to_start() {
        let path = std::env::temp_dir().join("tachyx-bad-config.toml");
        std::fs::write(&path, "listen = \"not an address\"").unwrap();
        assert!(
            matches!(Config::load(&path), Err(ConfigError::Parse { .. })),
            "ignoring a file the user wrote would be worse than refusing to start"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let path = std::env::temp_dir().join("tachyx-unknown-key.toml");
        std::fs::write(&path, "outupt = \"eDP-1\"").unwrap();
        assert!(
            matches!(Config::load(&path), Err(ConfigError::Parse { .. })),
            "a misspelled key must not look like it took effect"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_for_everything_else() {
        let path = std::env::temp_dir().join("tachyx-partial-config.toml");
        std::fs::write(&path, "output = \"DP-2\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.output.as_deref(), Some("DP-2"));
        assert_eq!(config.max_bitrate, Config::default().max_bitrate);
        assert_eq!(config.listen, Config::default().listen);
        std::fs::remove_file(&path).ok();
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tachyx-config-{name}-{}/hostd.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("a directory"));
        path
    }

    fn relay(seed: u8, address: &str) -> RelayAddress {
        RelayAddress {
            address: address.to_owned(),
            key: Some(
                tachyx_crypto::DeviceKey::from_seed(&[seed; 32])
                    .expect("a key")
                    .id(),
            ),
        }
    }

    #[test]
    fn a_host_with_no_configuration_still_names_a_relay_it_can_pin() {
        let relays = Config::default().relays;
        assert_eq!(
            relays.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![PROVISIONAL_RELAY.to_owned()],
            "a host with no relay can pair with nobody, so the default is not none"
        );
        assert!(
            PROVISIONAL_RELAY.parse::<RelayAddress>().is_ok(),
            "an address without the relay's key cannot be pinned, and this one is \
             the default every fresh host starts with"
        );
    }

    #[test]
    fn a_relay_added_from_the_command_line_is_the_one_the_daemon_reads_back() {
        let path = scratch("add");
        let first = relay(1, "100.64.0.2:47990");
        let second = relay(2, "relay.example:47990");
        Config::add_relay(&path, &first).unwrap();
        Config::add_relay(&path, &second).unwrap();
        assert_eq!(Config::load(&path).unwrap().relays, vec![first, second]);
    }

    #[test]
    fn adding_a_relay_that_rotated_its_key_replaces_it_rather_than_doubling_it() {
        let path = scratch("rotate");
        Config::add_relay(&path, &relay(1, "100.64.0.2:47990")).unwrap();
        let rotated = relay(9, "100.64.0.2:47990");
        Config::add_relay(&path, &rotated).unwrap();
        assert_eq!(Config::load(&path).unwrap().relays, vec![rotated]);
    }

    #[test]
    fn editing_the_relays_keeps_everything_else_the_person_wrote() {
        let path = scratch("comments");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let written = "# the laptop\noutput = \"eDP-1\"\n\n# nothing above 20 Mbit/s\nmax_bitrate = 20000000\n";
        std::fs::write(&path, written).unwrap();
        Config::add_relay(&path, &relay(1, "100.64.0.2:47990")).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# the laptop"), "{after}");
        assert!(after.contains("# nothing above 20 Mbit/s"), "{after}");
        assert_eq!(Config::load(&path).unwrap().max_bitrate, 20_000_000);
    }

    #[test]
    fn removing_a_relay_that_is_not_there_says_so_and_writes_nothing() {
        let path = scratch("remove");
        Config::add_relay(&path, &relay(1, "100.64.0.2:47990")).unwrap();
        assert!(!Config::remove_relay(&path, "10.0.0.1:47990").unwrap());
        assert!(Config::remove_relay(&path, "100.64.0.2:47990").unwrap());
        assert!(Config::load(&path).unwrap().relays.is_empty());
    }

    #[test]
    fn an_edit_that_would_leave_a_file_the_daemon_refuses_is_refused_first() {
        let path = scratch("refuse");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "outupt = \"eDP-1\"\n").unwrap();
        assert!(
            matches!(
                Config::add_relay(&path, &relay(1, "100.64.0.2:47990")),
                Err(ConfigError::Edit { .. })
            ),
            "a misspelled key is a file the daemon will not start on, and the edit must not \
             write over it"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "outupt = \"eDP-1\"\n",
            "the file is left exactly as it was"
        );
    }

    #[test]
    fn a_relay_add_leaves_the_file_the_mode_it_found_it_in() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("mode");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "output = \"eDP-1\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Config::add_relay(&path, &relay(1, "100.64.0.2:47990")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the file that was written is the file that was there, and it kept its mode"
        );
    }

    #[test]
    fn a_relay_address_that_would_repaint_a_terminal_is_not_an_address() {
        assert!(check_address("100.64.0.2:47990").is_ok());
        assert!(check_address("relay.tachyx.example:47990").is_ok());
        assert!(check_address("[2001:db8::1]:47990").is_ok());
        assert!(check_address("").is_err());
        assert!(
            check_address("\u{1b}[41m\u{1b}[2JPWNED\naaa:1").is_err(),
            "`relay list` prints this back on a terminal that reads it as instructions"
        );
        assert!(check_address("a b:47990").is_err());
        assert!(
            check_address(&format!("{}:47990", "r".repeat(ADDRESS_LIMIT))).is_err(),
            "a longer address is a pairing link that will not draw"
        );

        let long = format!(
            "{}:47990#{}",
            "r".repeat(ADDRESS_LIMIT),
            relay(1, "x").key.expect("keyed").to_base32()
        );
        assert!(
            matches!(
                Config::add_relay(&scratch("long"), &long.parse().expect("a relay")),
                Err(ConfigError::Address)
            ),
            "the daemon checks it too: the CLI is not the only thing that can ask"
        );
    }

    #[test]
    fn a_host_that_was_not_named_calls_itself_by_its_hostname() {
        assert!(!Config::default().host_name().is_empty());
        let named = Config {
            name: Some("desk".to_owned()),
            ..Config::default()
        };
        assert_eq!(named.host_name(), "desk");
    }
}
