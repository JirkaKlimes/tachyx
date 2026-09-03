//! Putting a code on offer, and finishing whatever answers it.
//!
//! [`spec/pairing.md`](../../../spec/pairing.md) is the document. Three flights
//! over a mailbox the relay cannot read, and the host reveals only after the
//! client's record has opened, so a wrong code learns nothing about this host.
//!
//! **One attempt per code.** The mailbox closes and the secret is gone the
//! moment the client's flight arrives, whether or not it opened.

use std::time::Duration;

use tachyx_crypto::code::PairingCodes;
use tachyx_crypto::store::PairedClient;
use tachyx_crypto::{CryptoError, HostPairing, HostRecord, Permissions};

use crate::control::{ControlError, PairingEvent, Response, Wire};
use crate::daemon::Daemon;
use crate::relay::{Mailbox, MailboxError};
use crate::show;

/// The most columns of a terminal a name from a peer is kept at.
///
/// A name is a label somebody else chose and this host prints. Long enough for
/// "Jiri's phone", short enough that no list is ruined by a peer that sent a
/// paragraph. Columns rather than characters, because sixty-four CJK
/// characters are a hundred and twenty-eight columns.
const NAME_LIMIT: usize = 64;

/// Runs one pairing, reporting each step to whoever asked for it.
///
/// Every failure that is not the CLI going away is reported as
/// [`PairingEvent::Failed`] rather than as an error, because the command that
/// asked for a pairing succeeded in asking: it is the pairing that did not
/// happen.
///
/// # Errors
/// If the connection to the CLI fails.
pub async fn run(
    daemon: &Daemon,
    permissions: Permissions,
    ttl: Duration,
    wire: &mut Wire,
) -> Result<(), ControlError> {
    match offer(daemon, permissions, ttl, wire).await {
        Ok(()) => Ok(()),
        Err(Fault::Wire(error)) => Err(error),
        Err(fault) => {
            wire.send(&Response::Pairing(PairingEvent::Failed {
                reason: fault.to_string(),
            }))
            .await
        }
    }
}

/// One pairing, from the mailbox opening to the device being written down.
async fn offer(
    daemon: &Daemon,
    permissions: Permissions,
    ttl: Duration,
    wire: &mut Wire,
) -> Result<(), Fault> {
    let config = daemon.config();
    let home = daemon.home().ok_or(Fault::NotServing)?;
    let relay = home.address().ok_or(Fault::NoRelay)?.clone();
    let mut mailbox = Mailbox::open(home).await?;

    let codes = PairingCodes::fresh(mailbox.nameplate());
    // Both forms are read off the codes before the exchange takes them: they are
    // neither `Copy` nor `Clone`, which is how *one attempt per code* is held by
    // the compiler rather than by a caller remembering.
    let typed = codes.typed_text();
    let uri = codes
        .invite(daemon.key.id(), relay.address.clone())
        .to_uri();
    let (pairing, flight_one) = HostPairing::start(codes)?;
    wire.send(&Response::Pairing(PairingEvent::Waiting { typed, uri }))
        .await?;
    mailbox.send(flight_one)?;

    // The relay has no message for somebody entering a mailbox, so the first
    // this host hears of a client is the client's own flight. See
    // `spec/relay.md`.
    let flight_two = match mailbox.recv(ttl).await {
        Ok(flight) => flight,
        Err(error) => {
            mailbox.release();
            return Err(error.into());
        }
    };
    wire.send(&Response::Pairing(PairingEvent::Joined)).await?;

    let record = HostRecord {
        device: daemon.key.id(),
        name: config.host_name(),
        home_relay: Some(relay.clone()),
        endpoints: mailbox.endpoints().to_vec(),
        permissions,
    };
    // Whatever this produces, the code is spent: `finish` takes the exchange by
    // value and the mailbox closes below either way.
    let nameplate = mailbox.nameplate();
    let (mut client, flight_three, words) = match pairing.finish(&flight_two, &record) {
        Ok(finished) => finished,
        Err(error) => {
            mailbox.release();
            tracing::warn!(
                %nameplate,
                %relay,
                "a pairing attempt did not have the code, and the code is spent"
            );
            return Err(error.into());
        }
    };
    mailbox.send(flight_three)?;
    mailbox.release();

    client.name = label(&client.name);
    daemon
        .clients
        .add(PairedClient::now(&client, permissions))?;
    tracing::info!(device = %client.device, "a device was paired");
    wire.send(&Response::Pairing(PairingEvent::Succeeded {
        device: client.device,
        name: client.name,
        words: words.to_string(),
    }))
    .await?;
    Ok(())
}

/// A name from a peer, made safe to print and short enough to list.
///
/// It arrives from a stranger and is written into a file this host prints to a
/// terminal. [`show::fit`] is what decides what survives that; this is the
/// moment it is applied, and the same filter runs again when the name is read
/// back, for a file that was written before this build existed.
#[must_use]
pub fn label(name: &str) -> String {
    let kept = show::fit(name, NAME_LIMIT);
    if kept.is_empty() {
        "unnamed".to_owned()
    } else {
        kept
    }
}

/// Why a pairing did not happen.
#[derive(Debug, thiserror::Error)]
enum Fault {
    /// The daemon is up but its endpoint is not, which is the moment between
    /// the control socket opening and the socket binding.
    #[error("this host is not serving sessions yet: try again in a moment")]
    NotServing,
    /// There is nowhere to open a mailbox.
    #[error(
        "no relay is configured, and pairing runs over a mailbox on a relay: \
         run `tachyx-hostd relay add <address:port#KEY>` first"
    )]
    NoRelay,
    /// The relay would not carry it.
    #[error(transparent)]
    Mailbox(#[from] MailboxError),
    /// The exchange refused, which is what a wrong code looks like.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// The CLI went away. Not reported to it, for obvious reasons.
    #[error(transparent)]
    Wire(#[from] ControlError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_from_a_peer_cannot_repaint_the_terminal_it_is_listed_on() {
        assert_eq!(label("phone"), "phone");
        assert_eq!(
            label("\u{1b}[2Jphone\u{7}"),
            "[2Jphone",
            "the escape character goes and the text it would have steered stays"
        );
        assert_eq!(label(&"a".repeat(500)).chars().count(), NAME_LIMIT);
        assert_eq!(label("   "), "unnamed");
        assert_eq!(label(""), "unnamed");
    }

    #[test]
    fn a_name_is_kept_at_the_columns_it_takes_rather_than_the_characters_it_has() {
        // Sixty-four of these are a hundred and twenty-eight columns, which is
        // a list nobody can read and a column nothing lines up in.
        let wide = "\u{4e2d}".repeat(64);
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(label(&wide).as_str()),
            NAME_LIMIT
        );
        assert_eq!(label(&wide).chars().count(), NAME_LIMIT / 2);
    }

    #[test]
    fn a_name_that_hides_or_reverses_what_it_is_listed_beside_is_filtered() {
        assert_eq!(
            label("phone\u{202e}enohp"),
            "phoneenohp",
            "a bidi override reverses everything printed after it, on the row and beyond"
        );
        assert_eq!(label("\u{2066}phone\u{2069}"), "phone", "bidi isolates");
        assert_eq!(label("pho\u{200b}ne"), "phone", "a zero width space");
        assert_eq!(label("\u{feff}phone"), "phone", "a byte order mark");
        assert_eq!(label("pho\u{ad}ne"), "phone", "a soft hyphen");
        assert_eq!(label("pho\u{e000}ne"), "phone", "a private use character");
        assert_eq!(label("one\u{2028}two"), "onetwo", "a line separator");
        assert_eq!(
            label("\u{301}phone"),
            "phone",
            "a combining mark with no base attaches to whatever was drawn last"
        );
        assert_eq!(
            label("e\u{301}phone"),
            "e\u{301}phone",
            "and one with a base is part of the name somebody chose"
        );
    }
}
