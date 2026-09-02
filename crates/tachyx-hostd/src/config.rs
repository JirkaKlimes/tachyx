//! What the daemon reads at startup.
//!
//! One TOML file, in the usual place, every field optional. A host with no
//! configuration at all serves its first output on the default port, which is
//! what makes the daemon worth starting before anyone has read any of this.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The port the daemon listens on when nothing says otherwise.
///
/// The client's own default must match this, in
/// `app/lib/src/session/link.dart`.
pub const DEFAULT_PORT: u16 = 47_989;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT),
            output: None,
            max_bitrate: 40_000_000,
            max_framerate: None,
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
    #[must_use]
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("tachyx").join("hostd.toml")
    }

    /// Where the host's own key and its record of paired clients live.
    #[must_use]
    pub fn state_directory() -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("tachyx")
    }
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
}
