//! Reaching a host by device: pairing over a real relay, then connecting.
//!
//! Nothing here is a mock. A `relayd` is bound on the loopback interface, the
//! host side is written out by hand so the test states what the protocol asks of
//! a host rather than what one daemon happens to do, and the client is the real
//! [`Link`] and the real [`Paired`].
//!
//! The far end runs on a thread and a runtime of its own, because the client
//! blocks the thread it is called on while it builds its own.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tachyx_client::capabilities;
use tachyx_crypto::code::PairingCodes;
use tachyx_crypto::store::{KnownHost, PairedClient};
use tachyx_crypto::{
    CryptoError, DeviceId, DeviceKey, HostPairing, HostRecord, PairedClients, Permissions,
    RelayAddress, ShortAuth,
};
use tachyx_driver::pairing::PairError;
use tachyx_driver::{known_hosts, Error, Link, Paired};
use tachyx_proto::control::{CodedSize, Size};
use tachyx_relay::{endpoint, Arrivals, HostRendezvous, RelayClient, Role, Routes};
use tachyx_relayd::Relay;
use tachyx_transport::{server_config, video_send_buffer, Host, Tuning};

/// Long enough for a loopback round trip and a busy scheduler, short enough that
/// a broken test fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the host calls itself, so the test can see the name reach the client's
/// own list.
const HOST_NAME: &str = "the test desk";

/// The surface the client says it has.
fn surface() -> Size {
    Size::new(1920, 1080).expect("a size")
}

fn asking() -> tachyx_proto::control::Capabilities {
    capabilities(surface(), std::num::NonZeroU16::new(60).expect("nonzero"))
}

fn tuning() -> Tuning {
    Tuning {
        video_buffer: video_send_buffer(CodedSize::new(1920, 1088).expect("a codeable size")),
        initial_mtu: tachyx_transport::RELAYED_MTU,
    }
}

/// A directory of this test's own, emptied first so a run never reads the last
/// one's keys.
fn scratch(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("tachyx-rendezvous-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A relay address that is written correctly and answers nothing.
///
/// Port one on the loopback interface: nothing binds it, and an unconnected UDP
/// socket never sees the refusal, so this is an address that times out rather
/// than one that fails to parse.
fn nowhere(seed: u8) -> RelayAddress {
    RelayAddress {
        address: "127.0.0.1:1".to_owned(),
        key: Some(DeviceKey::from_seed(&[seed; 32]).expect("a key").id()),
    }
}

/// A paired list holding one device.
fn paired_with(path: &Path, device: DeviceId) -> Arc<PairedClients> {
    let clients = Arc::new(PairedClients::open(path.to_owned()).expect("a paired list"));
    clients
        .add(PairedClient {
            device,
            name: "the test client".to_owned(),
            permissions: Permissions::default(),
            paired: 0,
        })
        .expect("the list is writable");
    clients
}

/// The far end of one test: whatever it builds, it reports through the channel
/// and then serves for as long as the process lives.
trait FarEnd<T>: FnOnce(Sender<T>) -> Pin<Box<dyn Future<Output = ()>>> + Send + 'static {}

impl<T, F> FarEnd<T> for F where
    F: FnOnce(Sender<T>) -> Pin<Box<dyn Future<Output = ()>>> + Send + 'static
{
}

/// Runs `far` on a thread and a runtime of its own, and hands back whatever it
/// reported before it began serving.
///
/// The thread is left running. A test binary ends when its tests do, and the
/// alternative is a shutdown channel through every far end for no gain.
fn spawn<T: Send + 'static>(far: impl FarEnd<T>) -> T {
    let (ready, built) = channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(far(ready));
    });
    built.recv_timeout(PATIENCE).expect("the far end came up")
}

/// A relay bound on the loopback interface, serving for as long as the process
/// lives.
fn relay_at(directory: &Path) -> (Arc<Relay>, RelayAddress) {
    let key = DeviceKey::load_or_create(&directory.join("relay")).expect("a relay key");
    let relay =
        Arc::new(Relay::bind((Ipv4Addr::LOCALHOST, 0).into(), &key).expect("a bound relay"));
    let address = relay.advertised().expect("a bound address");
    let serving = Arc::clone(&relay);
    tokio::spawn(async move { serving.run().await });
    (relay, address)
}

/// What the host put on its screen, and what it is.
struct Shown {
    /// Where the relay is, as a client is configured with it.
    relay: RelayAddress,
    /// The host's device.
    device: DeviceId,
    /// The eight digits, grouped as they are shown.
    typed: String,
    /// The link the QR beside them carries.
    uri: String,
}

/// Offers one pairing and reports what the exchange produced.
///
/// The whole host half by hand: a mailbox on the relay, both codes on offer, and
/// the record revealed only after the client's has opened. `endpoints` goes into
/// that record, which is what a later `Link::connect` reads back.
fn offering(
    directory: PathBuf,
    endpoints: Vec<SocketAddr>,
    outcome: Sender<Result<(DeviceId, ShortAuth), String>>,
) -> impl FarEnd<Shown> {
    move |ready: Sender<Shown>| {
        Box::pin(async move {
            let (_relay, address) = relay_at(&directory);
            let key = Arc::new(DeviceKey::load_or_create(&directory.join("host")).expect("a key"));
            let routes = Arc::new(Routes::new());
            let (endpoint, socket) =
                endpoint((Ipv4Addr::LOCALHOST, 0).into(), None, &routes).expect("an endpoint");
            let mailbox = RelayClient::open(
                address.clone(),
                Role::Client,
                Arc::clone(&key),
                endpoint,
                socket,
            );
            mailbox
                .wait_observed(PATIENCE)
                .await
                .expect("the host reaches the relay");
            let nameplate = mailbox.allocate().await.expect("a mailbox");

            let codes = PairingCodes::fresh(nameplate);
            // Read off the codes before the exchange takes them: `start` consumes
            // the draw, which is how one attempt per code is held by the
            // compiler.
            let typed = codes.typed_text();
            let uri = codes.invite(key.id(), address.address.clone()).to_uri();
            let (pairing, flight_one) = HostPairing::start(codes).expect("a pairing starts");
            mailbox.mail(flight_one).expect("the flight goes out");
            ready
                .send(Shown {
                    relay: address.clone(),
                    device: key.id(),
                    typed,
                    uri,
                })
                .expect("the test is waiting for the code");

            let flight_two = mailbox
                .next_mail(PATIENCE)
                .await
                .expect("the client's flight arrives");
            let record = HostRecord {
                device: key.id(),
                name: HOST_NAME.to_owned(),
                home_relay: Some(address),
                endpoints,
                permissions: Permissions::default(),
            };
            let said = match pairing.finish(&flight_two, &record) {
                Ok((client, flight_three, words)) => {
                    mailbox.mail(flight_three).expect("the flight goes out");
                    Ok((client.device, words))
                }
                // A wrong code reveals nothing: the host seals no record and
                // burns the mailbox, so the client sees only silence.
                Err(error) => Err(error.to_string()),
            };
            mailbox.release().expect("the mailbox closes");
            let _ = outcome.send(said);
            std::future::pending::<()>().await;
        })
    }
}

/// Where the client's own state lives, and the host it is paired with.
fn remembers(state: &Path, host: KnownHost) {
    known_hosts(state)
        .expect("a host list")
        .remember(host)
        .expect("the list is writable");
}

#[test]
fn a_typed_code_pairs_this_client_with_the_host_that_showed_it() {
    let state = scratch("typed");
    let (outcome, said) = channel();
    let shown = spawn(offering(state.join("far"), Vec::new(), outcome));

    let paired = Paired::run(
        &shown.typed,
        std::slice::from_ref(&shown.relay),
        &state.join("client"),
        "the test phone",
    )
    .expect("the code the host showed is the code the host takes");

    let (learned, host_words) = said
        .recv_timeout(PATIENCE)
        .expect("the host reported")
        .expect("the host finished the exchange");
    println!(
        "\n  the host learned   {learned}\n  \
           the client learned {} ({})\n  \
           short auth         {host_words}\n",
        paired.host.device, paired.host.name,
    );
    assert_eq!(paired.host.device, shown.device);
    assert_eq!(paired.host.name, HOST_NAME);
    assert_eq!(
        paired.words, host_words,
        "both screens show the same words, or the relay was in the middle"
    );

    let kept = known_hosts(&state.join("client"))
        .expect("a host list")
        .get(shown.device)
        .expect("a pairing that worked is a pairing the next run can use");
    assert_eq!(kept.device, shown.device);
    assert_eq!(kept.name, HOST_NAME);
    assert_eq!(
        kept.relay.map(|relay| relay.address),
        Some(shown.relay.address),
        "the authenticated relay address arrives inside the pairing"
    );
    assert_eq!(learned, kept_device(&state.join("client")));
}

/// This client's own device, which is what the host says it learned.
fn kept_device(state: &Path) -> DeviceId {
    DeviceKey::load_or_create(state).expect("a device key").id()
}

#[test]
fn a_scanned_link_pairs_and_carries_the_host_it_promised() {
    let state = scratch("scanned");
    let (outcome, said) = channel();
    let shown = spawn(offering(state.join("far"), Vec::new(), outcome));

    let paired = Paired::run(
        &shown.uri,
        std::slice::from_ref(&shown.relay),
        &state.join("client"),
        "the test phone",
    )
    .expect("a link pastes whole");

    said.recv_timeout(PATIENCE)
        .expect("the host reported")
        .expect("the host finished the exchange");
    assert_eq!(paired.host.device, shown.device);
}

#[test]
fn a_wrong_code_never_learns_who_the_host_was_and_says_what_to_do() {
    let state = scratch("wrong");
    let client = state.join("client");
    let (outcome, said) = channel();
    let shown = spawn(offering(state.join("far"), Vec::new(), outcome));

    // The same mailbox, one digit out. The host has one attempt to spend and
    // this spends it.
    let (nameplate, digits) = shown.typed.split_once('-').expect("a nameplate");
    let digits: String = digits.chars().filter(char::is_ascii_digit).collect();
    let wrong = format!(
        "{nameplate}-{:08}",
        (digits.parse::<u32>().expect("eight digits") + 1) % 100_000_000
    );

    let refused = Paired::run(&wrong, std::slice::from_ref(&shown.relay), &client, "the test phone")
        .expect_err("a wrong code is not a pairing");
    let host_said = said
        .recv_timeout(PATIENCE)
        .expect("the host reported")
        .expect_err("the host did not have that code");

    println!("\n  the host said      {host_said}\n  the client said    {refused}\n");
    assert!(
        matches!(*refused.source, PairError::Silent),
        "the host seals nothing on a wrong code, so silence is all there is: {refused:?}"
    );
    assert!(
        refused.spent,
        "the guess left this client, so the code must never be offered again"
    );
    let sentence = refused.to_string();
    assert!(
        sentence.contains("wrong") && sentence.contains("new one"),
        "a person has to be told the code may have been wrong and that a code is \
         good for one attempt, and got: {sentence}"
    );
    assert!(
        known_hosts(&client)
            .expect("a host list")
            .get(shown.device)
            .is_none(),
        "a wrong code must not learn the host's identifier"
    );
}

#[test]
fn a_code_for_a_mailbox_that_is_not_there_says_so() {
    let state = scratch("nomailbox");
    let (outcome, _said) = channel();
    let shown = spawn(offering(state.join("far"), Vec::new(), outcome));

    // A nameplate the relay has not allocated. Nothing is offered and no attempt
    // is spent: the relay refuses the join before a flight is written.
    let elsewhere = format!("{}-49218306", u16::from(u8::MAX) + 7);
    let refused = Paired::run(
        &elsewhere,
        std::slice::from_ref(&shown.relay),
        &state.join("client"),
        "the test phone",
    )
    .expect_err("no mailbox is not a pairing");

    assert!(
        refused
            .to_string()
            .contains("that pairing code names no open mailbox"),
        "the relay's own words are what a person can act on, and got: {refused}"
    );
    assert!(
        !refused.spent,
        "the join was refused before any flight left, so nothing was spent"
    );
}

#[test]
fn text_that_is_not_a_code_is_refused_before_anything_is_dialled() {
    let refused = Paired::run(
        "the wrong thing entirely",
        &[nowhere(1)],
        &scratch("nonsense"),
        "the test phone",
    )
    .expect_err("that is not a code");
    assert!(matches!(
        *refused.source,
        PairError::Exchange(CryptoError::NotACode)
    ));
    assert!(!refused.spent);
}

#[test]
fn pairing_with_no_relay_configured_says_which_thing_is_missing() {
    let refused = Paired::run("7-4921-8306", &[], &scratch("norelay"), "the test phone")
        .expect_err("a mailbox lives on a relay");
    assert!(matches!(*refused.source, PairError::NoRelay), "{refused:?}");
    assert!(!refused.spent);
}

/// A host on the wire, listening, with no relay anywhere near it.
fn listening(directory: PathBuf) -> impl FarEnd<(DeviceId, SocketAddr)> {
    move |ready: Sender<(DeviceId, SocketAddr)>| {
        Box::pin(async move {
            let key = DeviceKey::load_or_create(&directory.join("host")).expect("a host key");
            let client = DeviceKey::load_or_create(&directory.join("client")).expect("a key");
            let clients = paired_with(&directory.join("clients.toml"), client.id());
            let host = Host::bind(
                (Ipv4Addr::LOCALHOST, 0).into(),
                &key,
                clients,
                Tuning::for_stream(CodedSize::new(1920, 1088).expect("codeable")),
            )
            .expect("a bound host");
            ready
                .send((key.id(), host.local_address().expect("a bound address")))
                .expect("the test waits for the address");
            // Held, not dropped. Dropping a `HostSession` drops its
            // connection, and quinn closes a dropped connection with an empty
            // reason, which the client reads as a refusal.
            let mut open = Vec::new();
            loop {
                match host.accept().await {
                    Ok(session) => open.push(session),
                    Err(error) => tracing::debug!(%error, "the host stopped accepting"),
                }
            }
        })
    }
}

#[test]
fn a_host_at_an_address_it_gave_at_pairing_is_reached_without_a_relay() {
    let state = scratch("direct");
    let client = state.join("far").join("client");
    let (device, at) = spawn(listening(state.join("far")));

    remembers(
        &client,
        KnownHost {
            device,
            name: HOST_NAME.to_owned(),
            // A relay that answers nothing. Reaching the host anyway is the
            // whole assertion: a session that needed this would have taken the
            // five seconds it takes to give up on it.
            relay: Some(nowhere(9)),
            endpoints: vec![at],
            permissions: Permissions::default(),
        },
    );

    let link = Link::connect(device, &client, &[], asking())
        .expect("an address the host gave is an address the client tries first");
    assert!(link.stats().ended.is_none(), "the session is live");
}

#[test]
fn a_host_this_client_has_not_paired_with_is_refused_before_a_packet_is_sent() {
    let state = scratch("unknown");
    let stranger = DeviceKey::from_seed(&[42u8; 32]).expect("a key").id();
    let refused = Link::connect(stranger, &state, &[], asking())
        .expect_err("an address is not an identity and this client has neither");
    assert!(matches!(refused, Error::Unknown { .. }), "{refused:?}");
    assert!(refused.to_string().contains("has not paired"));
}

/// A host reachable only through a relay: registered on it, answering offers,
/// and accepting the session the punch or the splice carries.
fn reachable(directory: PathBuf) -> impl FarEnd<(DeviceId, RelayAddress)> {
    move |ready: Sender<(DeviceId, RelayAddress)>| {
        Box::pin(async move {
            let (_relay, address) = relay_at(&directory);
            let key = Arc::new(DeviceKey::load_or_create(&directory.join("host")).expect("a key"));
            let client = DeviceKey::load_or_create(&directory.join("client")).expect("a key");
            let clients = paired_with(&directory.join("clients.toml"), client.id());
            let server =
                server_config(&key, Arc::clone(&clients), tuning()).expect("a configuration");
            let routes = Arc::new(Routes::new());
            let (endpoint, socket) =
                endpoint((Ipv4Addr::LOCALHOST, 0).into(), Some(server), &routes)
                    .expect("an endpoint");
            let relay_client = Arc::new(RelayClient::open(
                address.clone(),
                Role::Host,
                Arc::clone(&key),
                endpoint.clone(),
                socket,
            ));
            relay_client
                .wait_observed(PATIENCE)
                .await
                .expect("the host reaches the relay");

            let arrivals = Arc::new(Arrivals::new());
            let listening = Host::on_endpoint(endpoint.clone(), tuning());
            let noticing = Arc::clone(&arrivals);
            tokio::spawn(async move {
                // Held, not dropped. Dropping a `HostSession` drops its
                // connection, and quinn closes a dropped connection with an
                // empty reason, which the client reads as a refusal.
                let mut open = Vec::new();
                loop {
                    match listening.accept().await {
                        Ok(session) => {
                            noticing.note(session.peer);
                            open.push(session);
                        }
                        Err(error) => tracing::debug!(%error, "the host stopped accepting"),
                    }
                }
            });
            let answering = HostRendezvous::new(
                Arc::clone(&key),
                clients,
                Arc::clone(&relay_client),
                endpoint,
                arrivals,
                tuning(),
            );
            // A host is reachable only once the relay has read its `Register`,
            // and `Observed` arrives before that, because the relay greets
            // before it reads. The client's five second deadline covers the
            // window; nothing in the protocol acknowledges a registration.
            ready
                .send((key.id(), address))
                .expect("the test waits for the relay");
            answering.serve(&[]).await;
            std::future::pending::<()>().await;
        })
    }
}

#[test]
fn a_host_whose_addresses_have_all_moved_is_found_through_its_relay() {
    let state = scratch("relayed");
    let client = state.join("far").join("client");
    let (device, relay) = spawn(reachable(state.join("far")));

    remembers(
        &client,
        KnownHost {
            device,
            name: HOST_NAME.to_owned(),
            relay: Some(relay),
            // Where the host was when it paired, and is not now.
            endpoints: vec![(Ipv4Addr::LOCALHOST, 1).into()],
            permissions: Permissions::default(),
        },
    );

    let link = Link::connect(device, &client, &[], asking())
        .expect("a host that has moved is still reachable through the relay it registered on");
    assert!(link.stats().ended.is_none(), "the session is live");
}
