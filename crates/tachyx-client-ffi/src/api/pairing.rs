//! Meeting a host for the first time, and the hosts already met.
//!
//! [`spec/pairing.md`](../../../../spec/pairing.md) is the document. What Dart
//! needs to know of it is small: the user hands over the code the host is
//! showing, this runs the exchange, and afterwards the host is in the list.
//!
//! Nothing here decides anything. Every rule lives in `tachyx-driver` and
//! `tachyx-crypto`, where a test reaches it without a Dart runtime.

use std::path::Path;

use tachyx_crypto::store::KnownHost;
use tachyx_crypto::{DeviceId, DeviceKey};
use tachyx_driver::Paired;

/// A host this client has paired with.
#[derive(Debug, Clone)]
pub struct KnownHostView {
    /// The identifier in full, as base32. This is what [`connect`] is given and
    /// it is the only half of a host that is ever compared.
    ///
    /// [`connect`]: crate::api::session::connect
    pub device: String,
    /// The first twelve characters of it, grouped, for a person reading a list.
    pub short: String,
    /// What the host calls itself. Chosen by the peer, so it names a row and
    /// decides nothing.
    pub name: String,
    /// Where to look when the host is not on this network.
    pub relay: Option<String>,
    /// Addresses worth trying before the relay is asked.
    pub endpoints: Vec<String>,
    /// Whether the host granted input as well as a picture.
    pub control: bool,
    /// Whether a session opens without somebody at the host accepting it.
    pub unattended: bool,
}

impl From<KnownHost> for KnownHostView {
    fn from(host: KnownHost) -> Self {
        Self {
            device: host.device.to_base32(),
            short: host.device.to_string(),
            name: host.name,
            relay: host.relay.map(|relay| relay.to_string()),
            endpoints: host
                .endpoints
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            control: host.permissions.control,
            unattended: host.permissions.unattended,
        }
    }
}

/// What a finished pairing produced.
#[derive(Debug, Clone)]
pub struct PairedHost {
    /// The host's identifier in full.
    pub device: String,
    /// The short form, for a person.
    pub short: String,
    /// What the host calls itself.
    pub name: String,
    /// The three symbols both ends show.
    ///
    /// A check for somebody who worries the code was overheard, and not one the
    /// protocol makes: the exchange has already decided by the time these can be
    /// compared. So they are shown, and nothing waits on them.
    pub words: String,
}

/// The hosts this client has paired with.
///
/// `state_directory` is this client's own directory, the one its device key
/// lives in.
///
/// # Errors
/// If the list cannot be read. A list that will not parse is an error rather
/// than an empty list, because a client that silently forgot every host would
/// send the user back through pairing for a fault that is not theirs.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
pub fn known_hosts(state_directory: String) -> anyhow::Result<Vec<KnownHostView>> {
    Ok(tachyx_driver::known_hosts(Path::new(&state_directory))?
        .list()
        .into_iter()
        .map(KnownHostView::from)
        .collect())
}

/// Forgets `device`, and reports whether it was there.
///
/// This end only. The host keeps its own record until somebody revokes this
/// client on it, so forgetting here stops this client dialling and does not
/// stop the host answering.
///
/// # Errors
/// If `device` is not an identifier, or the list cannot be written.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
pub fn forget_host(state_directory: String, device: String) -> anyhow::Result<bool> {
    Ok(tachyx_driver::known_hosts(Path::new(&state_directory))?
        .forget(DeviceId::parse(&device)?)?)
}

/// This client's own identifier, which is what a host pairs with.
///
/// Created on the first call, so a fresh install has one as soon as anything
/// asks.
///
/// # Errors
/// If the directory cannot be read or written.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
pub fn device_id(state_directory: String) -> anyhow::Result<String> {
    Ok(DeviceKey::load_or_create(Path::new(&state_directory))?
        .id()
        .to_base32())
}

/// How one pairing attempt ended.
///
/// A struct rather than a thrown error, because the failure carries a decision
/// as well as a sentence: [`PairReport::spent`] says whether the code went down
/// with the attempt, and the caller has to read it to hold *one attempt per
/// code* at this end.
#[derive(Debug, Clone)]
pub struct PairReport {
    /// The host, when the pairing worked.
    pub host: Option<PairedHost>,
    /// What went wrong, when it did not, in the words a person can act on.
    pub fault: Option<String>,
    /// Whether the code is gone either way.
    ///
    /// True the moment the guess left this device, whatever happened after,
    /// and always true on success. A spent code must never be offered again:
    /// against a hostile relay, every re-offer of the same code is a free
    /// online guess at it, charged to whoever retypes it.
    pub spent: bool,
}

/// Runs one pairing, from the text the user gave.
///
/// `code` is either the typed code, `7-4921-8306`, or a whole
/// `tachyx://pair?c=...` link. Deciding which is not Dart's job: a link also
/// names the host, and a host that is not that one then fails the pairing.
///
/// `name` is what this client calls itself in the host's list.
///
/// The caller must not offer a code this reported [`PairReport::spent`] for
/// again, and the interface has to say so rather than let the user retype it,
/// because a retyped spent code hands an attacker another guess.
#[expect(
    clippy::needless_pass_by_value,
    reason = "flutter_rust_bridge decodes an owned string out of the Dart side"
)]
#[must_use]
pub fn pair(
    code: String,
    relays: Vec<String>,
    state_directory: String,
    name: String,
) -> PairReport {
    let parsed = match crate::api::session::relay_list(&relays) {
        Ok(parsed) => parsed,
        Err(error) => {
            return PairReport {
                host: None,
                fault: Some(error.to_string()),
                spent: false,
            };
        }
    };
    match Paired::run(&code, &parsed, Path::new(&state_directory), &name) {
        Ok(paired) => PairReport {
            host: Some(PairedHost {
                device: paired.host.device.to_base32(),
                short: paired.host.device.to_string(),
                name: paired.host.name,
                words: paired.words.to_string(),
            }),
            fault: None,
            spent: true,
        },
        Err(fault) => PairReport {
            host: None,
            fault: Some(fault.to_string()),
            spent: fault.spent,
        },
    }
}
