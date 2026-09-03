//! The client's half of a pairing.
//!
//! [`spec/pairing.md`](../../../spec/pairing.md) is the document. Three flights
//! over a mailbox on a relay that cannot read them. The client answers the
//! host's key exchange, reveals itself sealed, and learns who the host is only
//! after its own record has opened.
//!
//! Nothing here draws a code. The host draws both codes and this reads one of
//! them, which is why the only text that comes in is what the user typed or what
//! the camera read.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use quinn::Endpoint;
use tachyx_crypto::store::KnownHost;
use tachyx_crypto::{
    ClientPairing, ClientRecord, CryptoError, DeviceId, DeviceKey, HostRecord, PairingCode,
    PairingInvite, RelayAddress, ShortAuth,
};
use tachyx_relay::{Refusal, RelayClient, RelayError, Role, Routes, RoutingSocket};
use tokio::runtime::Builder;

use crate::known_hosts;

/// How long the relay is given to say where it sees this client.
///
/// It says so unprompted as soon as the connection is up, so this is really how
/// long the relay is given to answer at all.
const REACH: Duration = Duration::from_secs(5);

/// How long one flight is waited for.
///
/// Flight one is already in the mailbox when the code is read, so what this
/// really covers is flight three: two hops through a relay, and a host doing
/// microseconds of arithmetic between them. Eight seconds is several times the
/// worst of that on a bad mobile link.
///
/// It is also how long a mistyped code takes to fail, and that cannot be made
/// shorter here. A host that did not have the code seals nothing and burns the
/// mailbox, and the relay tells the other party nothing when a mailbox closes,
/// so silence is the only thing a wrong code looks like from this end.
const FLIGHT: Duration = Duration::from_secs(8);

/// Runs the three flights against one relay.
///
/// `expect` is the host identifier a scanned link carried, and `None` for a
/// typed code. It is passed through to [`ClientPairing`], which refuses a host
/// that is not the one the link promised.
///
/// `endpoint` and `socket` are the one endpoint everything else runs on. The
/// relay connection shares it with the session on purpose: the address the relay
/// observes is then the address the host will be reached at.
///
/// One call is one attempt. Whatever this returns, the code is spent: the host
/// burns the mailbox on the first flight that arrives.
///
/// # Errors
/// If the relay cannot be reached, refuses, says nothing in time, or the code
/// was not the one the host is showing.
pub async fn pair(
    key: &Arc<DeviceKey>,
    relay: &RelayAddress,
    endpoint: &Endpoint,
    socket: &Arc<RoutingSocket>,
    code: &PairingCode,
    expect: Option<DeviceId>,
    me: &ClientRecord,
) -> Result<(HostRecord, ShortAuth), PairFault> {
    let client = RelayClient::open(
        relay.clone(),
        Role::Client,
        Arc::clone(key),
        endpoint.clone(),
        Arc::clone(socket),
    );
    // Everything up to the moment flight two leaves spends nothing: no guess
    // has been made, so a failure here may be retried with the same code and
    // [`PairFault::spent`] says so.
    let fresh = |error| PairFault::fresh(said(relay, error));
    client.wait_observed(REACH).await.map_err(fresh)?;
    client.join(code.nameplate).map_err(fresh)?;

    let flight_one = client.next_mail(FLIGHT).await.map_err(fresh)?;
    let (pairing, flight_two) = ClientPairing::answer(code, &flight_one, expect, me)
        .map_err(|error| PairFault::fresh(PairError::Exchange(error)))?;
    client.mail(flight_two).map_err(fresh)?;

    // Flight two is on its way, and flight two is the guess. From here every
    // failure is a spent code, because whoever is on the other side of the
    // mailbox, honest or not, may have read it.
    let spent = |error| PairFault::spent(said(relay, error));
    let flight_three = client.next_mail(FLIGHT).await.map_err(spent)?;
    pairing
        .finish(&flight_three)
        .map_err(|error| PairFault::spent(PairError::Exchange(error)))
}

/// What went wrong, and whether the code went down with it.
///
/// `spent` is the client's own bookkeeping of *one attempt per code*, and it is
/// the half of that rule only the client can hold. Against an honest relay the
/// mailbox is structurally one attempt: the first join owns it and its death
/// closes it. Against a hostile relay nothing the host does can help, because
/// the party that would report a guess is the attacker; what stops a wrong
/// guess per user retry is the client refusing to send the same code twice,
/// and this flag is what that refusal reads.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct PairFault {
    /// Whether flight two, the guess, left this client. Once it has, the code
    /// must never be offered again, whatever the failure was.
    pub spent: bool,
    /// What actually went wrong.
    #[source]
    pub source: Box<PairError>,
}

impl PairFault {
    /// A failure from before any guess left. The same code may be retried.
    fn fresh(source: PairError) -> Self {
        Self {
            spent: false,
            source: Box::new(source),
        }
    }

    /// A failure from after the guess left. The code is gone.
    fn spent(source: PairError) -> Self {
        Self {
            spent: true,
            source: Box::new(source),
        }
    }
}

/// What a finished pairing left behind.
#[derive(Debug)]
pub struct Paired {
    /// The host, as it is now written down.
    pub host: KnownHost,
    /// The three symbols both ends show, for somebody who worries the code was
    /// overheard. Nothing waits on them: the exchange has already decided.
    pub words: ShortAuth,
}

impl Paired {
    /// Runs one pairing, from the text the user gave to the host written down.
    ///
    /// `code` is either the typed code or a whole `tachyx://pair?c=...` link.
    /// Deciding which is this function's job: a link also names the host, which
    /// makes a host that is not that one fail the pairing, and it names the
    /// relay the mailbox is on.
    ///
    /// **One relay is tried, not all of them.** A mailbox that is not there is
    /// refused before the code is offered, so trying the next relay would be
    /// free; a connection that dies part way through the exchange is not, and
    /// retrying that one would spend a second guess against a code that is only
    /// good for one. So the relay is chosen once: the one the link named, if it
    /// is configured, and otherwise the first configured. That is also what the
    /// host does, which opens its mailbox on the first relay it has.
    ///
    /// # Errors
    /// If the text is not a code, there is no relay, the relay cannot be reached
    /// or refuses, nothing answers, the code was wrong, or the host that was
    /// learned cannot be written down. [`PairFault::spent`] says whether the
    /// code went down with the failure, and a caller must not offer a spent one
    /// again.
    pub fn run(
        code: &str,
        relays: &[RelayAddress],
        state: &Path,
        name: &str,
    ) -> Result<Self, PairFault> {
        let (code, expect, hint) = read(code).map_err(PairFault::fresh)?;
        let relay = chosen(relays, hint.as_deref())
            .ok_or(PairFault::fresh(PairError::NoRelay))?
            .clone();
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| PairFault::fresh(PairError::Runtime(error)))?;
        runtime.block_on(async move {
            let key = Arc::new(
                DeviceKey::load_or_create(state)
                    .map_err(|error| PairFault::fresh(PairError::State(error)))?,
            );
            let routes = Arc::new(Routes::new());
            let (endpoint, socket) = crate::bind(&routes)
                .map_err(|error| PairFault::fresh(PairError::Local(Box::new(error))))?;
            let me = ClientRecord {
                device: key.id(),
                name: name.to_owned(),
            };
            let (record, words) =
                pair(&key, &relay, &endpoint, &socket, &code, expect, &me).await?;
            let host = KnownHost::from_record(&record);
            // The exchange succeeded, so the host has written this client down
            // and the code is spent whatever happens to the file: a caller that
            // retried the code here would be guessing against a mailbox that no
            // longer exists.
            known_hosts(state)
                .map_err(|error| PairFault::spent(PairError::State(error)))?
                .remember(host.clone())
                .map_err(|error| PairFault::spent(PairError::Store(error)))?;
            Ok(Self { host, words })
        })
    }
}

/// What the user gave, as the exchange needs it: the code, the host a link
/// promised, and where a link said the mailbox is.
///
/// A link is read first because a typed code cannot be mistaken for one: it
/// carries no scheme. The relay address inside a link is a hint and nothing
/// more. The link does not carry the relay's key, so a relay that is not already
/// configured cannot be dialled from it, and the authenticated copy arrives
/// inside the pairing anyway.
fn read(text: &str) -> Result<(PairingCode, Option<DeviceId>, Option<String>), PairError> {
    if let Ok(invite) = PairingInvite::parse(text) {
        return Ok((invite.code(), Some(invite.host), Some(invite.relay)));
    }
    PairingCode::parse_typed(text)
        .map(|code| (code, None, None))
        .map_err(PairError::Exchange)
}

/// The relay to open the mailbox on: the one a link named, if it is configured,
/// and otherwise the first configured.
fn chosen<'a>(relays: &'a [RelayAddress], hint: Option<&str>) -> Option<&'a RelayAddress> {
    relays
        .iter()
        .find(|relay| hint.is_some_and(|named| named == relay.address))
        .or_else(|| relays.first())
}

/// What the relay said, as the thing a person can act on.
///
/// The three are kept apart on purpose. "that pairing code names no open
/// mailbox" tells the user the host has stopped offering it; "cannot reach the
/// relay" tells them the network is the fault; silence tells them neither.
fn said(relay: &RelayAddress, error: RelayError) -> PairError {
    match error {
        RelayError::Refused(refusal) => PairError::Refused(refusal),
        RelayError::Silent => PairError::Silent,
        source => PairError::Unreachable {
            relay: relay.address.clone(),
            source: Box::new(source),
        },
    }
}

/// Why a pairing did not happen.
///
/// The failures a person can act on stay apart from each other. A wrong code, a
/// mailbox that is not there, a relay that cannot be reached and a silence are
/// four different things to do next, and one "pairing failed" would be none of
/// them.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// The client's own runtime would not start.
    #[error("the client cannot start its own runtime")]
    Runtime(#[source] std::io::Error),
    /// The client could not open the socket it would pair from. Nothing was
    /// asked of any relay.
    #[error("the client cannot open a socket to pair from")]
    Local(#[source] Box<RelayError>),
    /// This client's own state directory could not be read or written.
    #[error(transparent)]
    State(CryptoError),
    /// There is nowhere to open a mailbox.
    #[error("no relay is configured, and pairing runs over a mailbox on a relay")]
    NoRelay,
    /// The relay could not be reached, or the connection to it went away.
    #[error("cannot reach the relay at {relay}")]
    Unreachable {
        /// Which relay, as it is written in the settings.
        relay: String,
        /// What the relay client said.
        #[source]
        source: Box<RelayError>,
    },
    /// The relay refused, and said why. Its words, because a client told "that
    /// pairing code names no open mailbox" can act on it.
    #[error("the relay refused: {0}")]
    Refused(Refusal),
    /// Nothing answered inside the deadline.
    #[error(
        "nothing answered the code: it was wrong, or the host stopped offering it. \
         a code is good for one attempt, so ask the host for a new one"
    )]
    Silent,
    /// The exchange refused. A wrong code is the common one; a host that is not
    /// the one a scanned link named is the other.
    #[error(transparent)]
    Exchange(CryptoError),
    /// The pairing worked and the host could not be written down, so the next
    /// run would not know it.
    #[error("paired with the host, but it could not be written down")]
    Store(#[source] CryptoError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(address: &str, seed: u8) -> RelayAddress {
        RelayAddress {
            address: address.to_owned(),
            key: Some(DeviceKey::from_seed(&[seed; 32]).expect("a key").id()),
        }
    }

    #[test]
    fn a_typed_code_names_no_host_and_no_relay() {
        let (code, expect, hint) = read("7-4921-8306").expect("a code");
        assert_eq!(code.nameplate.get(), 7);
        assert_eq!(expect, None, "the typed form carries no identifier");
        assert_eq!(hint, None);
    }

    #[test]
    fn a_link_names_the_host_it_promises_and_the_mailbox_it_is_in() {
        let host = DeviceKey::from_seed(&[3u8; 32]).expect("a key").id();
        let uri = tachyx_crypto::code::PairingCodes::fresh(
            tachyx_crypto::Nameplate::new(9).expect("a nameplate"),
        )
        .invite(host, "100.64.0.2:47990".to_owned())
        .to_uri();
        let (code, expect, hint) = read(&uri).expect("a link");
        assert_eq!(code.nameplate.get(), 9);
        assert_eq!(expect, Some(host), "a scanned code is checked against this");
        assert_eq!(hint.as_deref(), Some("100.64.0.2:47990"));
    }

    #[test]
    fn text_that_is_neither_is_refused_without_being_quoted_back() {
        let refused = read("not a code").expect_err("not a code");
        assert!(
            matches!(refused, PairError::Exchange(CryptoError::NotACode)),
            "{refused:?}"
        );
        assert!(
            !refused.to_string().contains("not a code"),
            "a mistyped code is one character from a live secret: {refused}"
        );
    }

    #[test]
    fn the_relay_a_link_names_is_the_one_the_mailbox_is_opened_on() {
        let relays = [relay("first:47990", 1), relay("named:47990", 2)];
        assert_eq!(
            chosen(&relays, Some("named:47990")),
            Some(&relays[1]),
            "the link says where the mailbox is"
        );
        assert_eq!(
            chosen(&relays, Some("somewhere.else:47990")),
            Some(&relays[0]),
            "a relay with no key configured for it cannot be dialled, so the \
             configured one is tried and says whether the mailbox is there"
        );
        assert_eq!(chosen(&relays, None), Some(&relays[0]));
        assert_eq!(chosen(&[], None), None);
    }
}
