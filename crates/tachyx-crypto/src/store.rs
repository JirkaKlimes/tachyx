//! What each side keeps after a pairing.
//!
//! A host keeps the clients it has paired with and what each may do. A client
//! keeps the hosts it knows and where to look for them. Both are small TOML
//! files: a person can read one, `tachyx pairings list` prints one, and nobody
//! has more than a handful of rows.
//!
//! Every write goes to a temporary file and is renamed over the target, so a
//! crash during a write leaves the old list rather than half a new one.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::device::{random, DeviceId};
use crate::pair::{ClientRecord, HostRecord, Permissions};
use crate::{CryptoError, BASE32};

/// Where a relay is, and the key it must present when one is written down.
///
/// Written `address:port#KEY`, or bare `address:port`. The key travels in the
/// string because a relay address is configuration somebody pastes once, and
/// pinning on first sight would leave that one moment undefended for nothing.
///
/// The key is optional because of what checking it buys, which is metadata
/// privacy and nothing above it: see `spec/relay.md`. A session through a relay
/// is end to end between two pinned device keys either way. What a bare address
/// gives up is knowing whose relay it is on, so anyone who can intercept the
/// address learns who this device reaches and when, and can force the relayed
/// path. Once a key is written down it is always checked: there is no downgrade,
/// because the only unverified relay is one the operator wrote without a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAddress {
    /// A name or a literal address, with the port. Not resolved here.
    pub address: String,
    /// The key the relay must present, or `None` for a relay the operator
    /// chose not to pin.
    pub key: Option<DeviceId>,
}

impl fmt::Display for RelayAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.key {
            Some(key) => write!(f, "{}#{}", self.address, key.to_base32()),
            None => write!(f, "{}", self.address),
        }
    }
}

impl FromStr for RelayAddress {
    type Err = CryptoError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        let (address, key) = match text.rsplit_once('#') {
            // A `#` was written, so what follows it must be a key. Quietly
            // accepting a mistyped one as no key at all would drop the pin the
            // operator meant to set.
            Some((address, key)) => (address, Some(DeviceId::parse(key)?)),
            None => (text, None),
        };
        if address.is_empty() {
            return Err(CryptoError::unreadable("a relay address", text));
        }
        Ok(Self {
            address: address.to_owned(),
            key,
        })
    }
}

/// Written as the one string a person pastes, on the wire as well as in a file.
/// It travels once, inside a pairing, so the few bytes a text form costs there
/// buy one representation instead of two.
impl Serialize for RelayAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RelayAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A device identifier as the base32 a file shows, rather than as the 32 bytes
/// the wire carries.
mod as_text {
    use super::{Deserialize as _, Deserializer, DeviceId, Serializer};

    pub(super) fn serialize<S: Serializer>(
        id: &DeviceId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_base32())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DeviceId, D::Error> {
        let text = String::deserialize(deserializer)?;
        DeviceId::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// One client this host has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedClient {
    /// Which device. This is what a TLS handshake is compared against.
    #[serde(with = "as_text")]
    pub device: DeviceId,
    /// What the client called itself, for a person reading the list.
    pub name: String,
    /// What it may do.
    #[serde(flatten)]
    pub permissions: Permissions,
    /// When it was paired, in seconds since the Unix epoch.
    pub paired: u64,
}

impl PairedClient {
    /// Records `record` as paired now, with `permissions`.
    #[must_use]
    pub fn now(record: &ClientRecord, permissions: Permissions) -> Self {
        Self {
            device: record.device,
            name: record.name.clone(),
            permissions,
            paired: now(),
        }
    }
}

/// One host this client knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHost {
    /// Which device. This is what a TLS handshake is compared against.
    #[serde(with = "as_text")]
    pub device: DeviceId,
    /// What the host called itself.
    pub name: String,
    /// Where to look when it is not on this network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayAddress>,
    /// Addresses worth trying before the relay is asked.
    #[serde(default)]
    pub endpoints: Vec<SocketAddr>,
    /// What the host granted, as it said at pairing. The host enforces this and
    /// the client only reports it, so it is a label and not a permission.
    #[serde(flatten)]
    pub permissions: Permissions,
}

impl KnownHost {
    /// What a completed pairing learned.
    #[must_use]
    pub fn from_record(record: &HostRecord) -> Self {
        Self {
            device: record.device,
            name: record.name.clone(),
            relay: record.home_relay.clone(),
            endpoints: record.endpoints.clone(),
            permissions: record.permissions,
        }
    }
}

/// The file the host keeps its paired clients in.
#[derive(Debug, Serialize, Deserialize, Default)]
struct ClientFile {
    #[serde(default, rename = "client")]
    clients: Vec<PairedClient>,
}

/// The file the client keeps its hosts in.
#[derive(Debug, Serialize, Deserialize, Default)]
struct HostFile {
    #[serde(default, rename = "host")]
    hosts: Vec<KnownHost>,
}

/// The clients this host has paired with.
///
/// Shared behind a lock because the daemon reads it inside a TLS handshake while
/// the control socket may be rewriting it.
#[derive(Debug)]
pub struct PairedClients {
    path: PathBuf,
    clients: Mutex<Vec<PairedClient>>,
}

impl PairedClients {
    /// Opens the list at `path`, treating a missing file as an empty one.
    ///
    /// # Errors
    /// If the file exists and is not a list this build reads. A file somebody
    /// hand-edited into nonsense refuses to start rather than silently granting
    /// nobody access, or worse, everybody.
    pub fn open(path: PathBuf) -> Result<Self, CryptoError> {
        let file: ClientFile = read(&path)?;
        Ok(Self {
            path,
            clients: Mutex::new(file.clients),
        })
    }

    /// Whether `device` is paired at all, which is what a handshake asks.
    #[must_use]
    pub fn get(&self, device: DeviceId) -> Option<PairedClient> {
        guard(&self.clients)
            .iter()
            .find(|client| client.device == device)
            .cloned()
    }

    /// Every paired client, most recently paired first.
    #[must_use]
    pub fn list(&self) -> Vec<PairedClient> {
        let mut clients = guard(&self.clients).clone();
        clients.sort_unstable_by_key(|client| std::cmp::Reverse(client.paired));
        clients
    }

    /// Whether nothing is paired, which is what makes a daemon say so at start.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        guard(&self.clients).is_empty()
    }

    /// Adds `client`, replacing any earlier record of the same device.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn add(&self, client: PairedClient) -> Result<(), CryptoError> {
        self.edit(|clients| {
            clients.retain(|held| held.device != client.device);
            clients.push(client);
            true
        })
        .map(|_| ())
    }

    /// Forgets `device`, reporting whether it was there.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn remove(&self, device: DeviceId) -> Result<bool, CryptoError> {
        self.edit(|clients| {
            let before = clients.len();
            clients.retain(|held| held.device != device);
            clients.len() != before
        })
    }

    /// Changes what `device` may do, reporting whether it was there.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn set_permissions(
        &self,
        device: DeviceId,
        permissions: Permissions,
    ) -> Result<bool, CryptoError> {
        self.edit(|clients| {
            clients
                .iter_mut()
                .find(|held| held.device == device)
                .is_some_and(|held| {
                    held.permissions = permissions;
                    true
                })
        })
    }

    /// Renames `device`, reporting whether it was there.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn rename(&self, device: DeviceId, name: String) -> Result<bool, CryptoError> {
        self.edit(|clients| {
            clients
                .iter_mut()
                .find(|held| held.device == device)
                .is_some_and(|held| {
                    held.name = name;
                    true
                })
        })
    }

    /// Applies `change` and writes the file when it changed something.
    ///
    /// The change is made on a copy and committed into the shared list only
    /// after the file is on disk. The other order leaves a device paired in
    /// memory that the caller was told did not pair, and a device revoked in
    /// memory that is still on disk, for the life of the process.
    fn edit(
        &self,
        change: impl FnOnce(&mut Vec<PairedClient>) -> bool,
    ) -> Result<bool, CryptoError> {
        let mut clients = guard(&self.clients);
        let mut next = clients.clone();
        if !change(&mut next) {
            return Ok(false);
        }
        write(
            &self.path,
            &ClientFile {
                clients: next.clone(),
            },
        )?;
        *clients = next;
        Ok(true)
    }
}

/// The hosts this client knows.
#[derive(Debug)]
pub struct KnownHosts {
    path: PathBuf,
    hosts: Mutex<Vec<KnownHost>>,
}

impl KnownHosts {
    /// Opens the list at `path`, treating a missing file as an empty one.
    ///
    /// # Errors
    /// If the file exists and is not a list this build reads.
    pub fn open(path: PathBuf) -> Result<Self, CryptoError> {
        let file: HostFile = read(&path)?;
        Ok(Self {
            path,
            hosts: Mutex::new(file.hosts),
        })
    }

    /// What is known about `device`.
    #[must_use]
    pub fn get(&self, device: DeviceId) -> Option<KnownHost> {
        guard(&self.hosts)
            .iter()
            .find(|host| host.device == device)
            .cloned()
    }

    /// Every known host, in the order they were learned.
    #[must_use]
    pub fn list(&self) -> Vec<KnownHost> {
        guard(&self.hosts).clone()
    }

    /// Keeps `host`, replacing any earlier record of the same device.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn remember(&self, host: KnownHost) -> Result<(), CryptoError> {
        let mut hosts = guard(&self.hosts);
        let mut next = hosts.clone();
        next.retain(|held| held.device != host.device);
        next.push(host);
        write(
            &self.path,
            &HostFile {
                hosts: next.clone(),
            },
        )?;
        *hosts = next;
        Ok(())
    }

    /// Forgets `device`, reporting whether it was there.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn forget(&self, device: DeviceId) -> Result<bool, CryptoError> {
        let mut hosts = guard(&self.hosts);
        let mut next = hosts.clone();
        next.retain(|held| held.device != device);
        if next.len() == hosts.len() {
            return Ok(false);
        }
        write(
            &self.path,
            &HostFile {
                hosts: next.clone(),
            },
        )?;
        *hosts = next;
        Ok(true)
    }
}

/// The list, whether or not a thread died holding it.
///
/// A poisoned lock here means a write panicked part way. The list in memory is
/// still the list, and refusing every later connection over it would be the
/// larger fault.
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reads a file, treating a missing one as empty.
fn read<T: for<'a> Deserialize<'a> + Default>(path: &Path) -> Result<T, CryptoError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(error.into()),
    };
    toml::from_str(&text).map_err(|source| CryptoError::Store {
        path: path.to_owned(),
        source,
    })
}

/// Writes a file whole, or leaves the old one.
///
/// A temporary beside the target and a rename over it, because a list of who may
/// reach this desktop is not a thing to leave half written.
///
/// Three things here are not decoration.
///
/// The temporary's name is random rather than the process identifier, and it is
/// opened with `create_new`. A predictable name that an existing file can
/// satisfy is two faults at once: `mode` applies only at creation, so a planted
/// 0666 temporary hands its mode to the list and the list becomes world
/// writable, which is anyone on the machine adding themselves to it; and a
/// planted symlink is followed, which writes the list through it and then
/// renames the symlink over the list.
///
/// The contents are flushed before the rename and the directory after it. A
/// rename can otherwise reach the disk while the bytes do not, and a
/// zero-length list reads as nobody paired.
///
/// A temporary that is left behind by a failure is removed, so a full disk does
/// not fill it further on every attempt.
fn write<T: Serialize>(path: &Path, value: &T) -> Result<(), CryptoError> {
    use std::io::Write as _;

    let text = toml::to_string_pretty(value).map_err(|error| CryptoError::Encoding {
        path: path.to_owned(),
        detail: error.to_string(),
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let temporary = path.with_extension(format!("tmp{}", BASE32.encode(&random::<8>())));
    let outcome = (|| -> Result<(), std::io::Error> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    Ok(outcome?)
}

/// Seconds since the Unix epoch, and zero on a clock set before it.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceKey;

    fn device(seed: u8) -> DeviceId {
        DeviceKey::from_seed(&[seed; 32]).expect("a key").id()
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tachyx-store-{name}-{}/list.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().expect("a directory"));
        path
    }

    fn client(seed: u8) -> PairedClient {
        PairedClient {
            device: device(seed),
            name: format!("phone {seed}"),
            permissions: Permissions::default(),
            paired: u64::from(seed),
        }
    }

    #[test]
    fn a_relay_address_survives_a_round_trip_through_its_text() {
        let relay = RelayAddress {
            address: "100.64.0.2:47990".to_owned(),
            key: Some(device(1)),
        };
        let text = relay.to_string();
        assert_eq!(text.parse::<RelayAddress>().unwrap(), relay);
        assert_eq!(
            format!("relay.example:47990#{}", device(1).to_base32())
                .parse::<RelayAddress>()
                .unwrap()
                .address,
            "relay.example:47990",
            "a name is taken as written and is not resolved here"
        );
    }

    #[test]
    fn a_relay_address_without_a_key_is_an_unpinned_relay_and_not_a_mistake() {
        let relay: RelayAddress = "100.64.0.2:47990".parse().unwrap();
        assert_eq!(relay.address, "100.64.0.2:47990");
        assert_eq!(relay.key, None);
        assert_eq!(
            relay.to_string(),
            "100.64.0.2:47990",
            "what was written comes back, so a file round trip keeps the choice"
        );
    }

    #[test]
    fn a_relay_address_with_a_broken_key_is_refused_rather_than_unpinned() {
        for written in [
            "#",
            &format!("#{}", device(1).to_base32()),
            "100.64.0.2:47990#not-a-key",
            "100.64.0.2:47990#",
        ] {
            assert!(
                written.parse::<RelayAddress>().is_err(),
                "{written:?}: a mistyped key must not quietly become no pin at all"
            );
        }
    }

    #[test]
    fn a_paired_client_survives_a_round_trip_through_the_file() {
        let path = scratch("clients");
        let store = PairedClients::open(path.clone()).unwrap();
        assert!(store.is_empty());
        store.add(client(1)).unwrap();
        store.add(client(2)).unwrap();

        let reopened = PairedClients::open(path).unwrap();
        assert_eq!(reopened.get(device(1)), Some(client(1)));
        assert_eq!(reopened.get(device(3)), None);
        assert_eq!(reopened.list().len(), 2);
    }

    #[test]
    fn pairing_the_same_device_twice_replaces_it_rather_than_doubling_it() {
        let store = PairedClients::open(scratch("replace")).unwrap();
        store.add(client(1)).unwrap();
        let renamed = PairedClient {
            name: "the same phone".to_owned(),
            ..client(1)
        };
        store.add(renamed.clone()).unwrap();
        assert_eq!(store.list(), vec![renamed]);
    }

    #[test]
    fn revoking_a_device_that_is_not_there_says_so_and_writes_nothing() {
        let store = PairedClients::open(scratch("revoke")).unwrap();
        store.add(client(1)).unwrap();
        assert!(!store.remove(device(9)).unwrap());
        assert!(store.remove(device(1)).unwrap());
        assert!(store.is_empty());
    }

    #[test]
    fn permissions_and_names_can_be_changed_in_place() {
        let store = PairedClients::open(scratch("edit")).unwrap();
        store.add(client(1)).unwrap();
        let view_only = Permissions {
            control: false,
            unattended: false,
        };
        assert!(store.set_permissions(device(1), view_only).unwrap());
        assert!(store.rename(device(1), "desk phone".to_owned()).unwrap());
        assert!(!store.rename(device(9), "nobody".to_owned()).unwrap());
        let held = store.get(device(1)).expect("still paired");
        assert_eq!(held.permissions, view_only);
        assert_eq!(held.name, "desk phone");
    }

    #[test]
    fn the_list_is_most_recently_paired_first() {
        let store = PairedClients::open(scratch("order")).unwrap();
        store.add(client(1)).unwrap();
        store.add(client(5)).unwrap();
        store.add(client(3)).unwrap();
        assert_eq!(
            store
                .list()
                .into_iter()
                .map(|c| c.paired)
                .collect::<Vec<_>>(),
            vec![5, 3, 1]
        );
    }

    #[test]
    fn a_known_host_survives_a_round_trip_through_the_file() {
        let path = scratch("hosts");
        let store = KnownHosts::open(path.clone()).unwrap();
        let host = KnownHost {
            device: device(4),
            name: "desk".to_owned(),
            relay: Some(RelayAddress {
                address: "100.64.0.2:47990".to_owned(),
                key: Some(device(5)),
            }),
            endpoints: vec!["10.0.0.4:47989".parse().unwrap()],
            permissions: Permissions::default(),
        };
        store.remember(host.clone()).unwrap();
        let reopened = KnownHosts::open(path).unwrap();
        assert_eq!(reopened.get(device(4)), Some(host));
        assert!(!reopened.forget(device(9)).unwrap());
        assert!(reopened.forget(device(4)).unwrap());
        assert!(reopened.list().is_empty());
    }

    #[test]
    fn a_file_somebody_edited_into_nonsense_refuses_rather_than_reading_as_empty() {
        let path = scratch("nonsense");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[client]]\ndevice = \"not a key\"\n").unwrap();
        assert!(
            matches!(PairedClients::open(path), Err(CryptoError::Store { .. })),
            "an unreadable list must not look like nobody is paired"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_write_that_failed_has_granted_nobody_anything() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("readonly");
        let store = PairedClients::open(path.clone()).unwrap();
        store.add(client(1)).unwrap();

        let directory = path.parent().expect("a directory");
        let was = std::fs::metadata(directory).unwrap().permissions();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = store.add(client(2));
        std::fs::set_permissions(directory, was).unwrap();

        assert!(refused.is_err(), "a read-only directory cannot be written");
        assert_eq!(
            store.get(device(2)),
            None,
            "a device the operator was told did not pair must not be paired anyway"
        );
        assert!(store.get(device(1)).is_some(), "and the old list is intact");
    }

    #[cfg(unix)]
    #[test]
    fn a_revoke_that_failed_has_revoked_nobody() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("readonly-revoke");
        let store = PairedClients::open(path.clone()).unwrap();
        store.add(client(1)).unwrap();

        let directory = path.parent().expect("a directory");
        let was = std::fs::metadata(directory).unwrap().permissions();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = store.remove(device(1));
        std::fs::set_permissions(directory, was).unwrap();

        assert!(refused.is_err());
        assert!(
            store.get(device(1)).is_some(),
            "a revocation that did not reach the disk has not happened, and saying \
             otherwise would have the operator believe a device was cut off"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_temporary_cannot_take_the_list_with_it() {
        // The temporary is named at random and opened with `create_new`, so a
        // file already sitting at the name it would have chosen is refused
        // rather than opened, and cannot hand its own mode or its symlink target
        // to the list. Nothing can plant a name that is not known, so what this
        // holds is the property that makes that true: two writes never reuse a
        // temporary name.
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("planted");
        let store = PairedClients::open(path.clone()).unwrap();
        for seed in 1..8u8 {
            store.add(client(seed)).unwrap();
        }
        let directory = path.parent().expect("a directory");
        let leftovers = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "every temporary was renamed or removed");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_list_is_written_readable_by_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("mode");
        let store = PairedClients::open(path.clone()).unwrap();
        store.add(client(1)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
