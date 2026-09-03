//! A real relay, real peers, and real sessions through both.
//!
//! Nothing here is a mock. A `relayd` is bound on the loopback interface and
//! every peer is a real quinn endpoint over the real routing socket, so what is
//! checked is the behaviour rather than a reading of the source.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tachyx_crypto::code::PairingCodes;
use tachyx_crypto::store::PairedClient;
use tachyx_crypto::{
    ClientPairing, ClientRecord, DeviceId, DeviceKey, HostPairing, HostRecord, PairedClients,
    Permissions, RelayAddress,
};
use tachyx_proto::control::{
    Capabilities, ClientHello, ClientMessage, Codec, Codecs, CodedSize, HostMessage, Size,
    StreamConfig,
};
use tachyx_relay::proto::{splice_frame, Offer, SPLICE_TAG};
use tachyx_relay::{
    endpoint, Arrivals, ClientRendezvous, HostRendezvous, Lane, Path, RelayClient, RelayError,
    Role, Routes,
};
use tachyx_relayd::Relay;
use tachyx_transport::{
    server_config, video_send_buffer, Client, ClientSession, Host, HostSession, Tuning,
};

/// Long enough for a loopback round trip and a busy scheduler, short enough that
/// a broken test fails rather than hangs.
const PATIENCE: Duration = Duration::from_secs(10);

/// A relay running on the loopback interface for the life of the test.
struct Loopback {
    address: RelayAddress,
    relay: Arc<Relay>,
    serving: tokio::task::JoinHandle<()>,
    directory: PathBuf,
}

impl Loopback {
    fn open(name: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("tachyx-relayd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let key = DeviceKey::load_or_create(&directory.join("relay")).expect("a relay key");
        let relay =
            Arc::new(Relay::bind((Ipv4Addr::LOCALHOST, 0).into(), &key).expect("a bound relay"));
        let address = relay.advertised().expect("a bound address");
        let running = Arc::clone(&relay);
        Self {
            address,
            relay,
            serving: tokio::spawn(async move { running.run().await }),
            directory,
        }
    }

    fn at(&self) -> SocketAddr {
        self.address.address.parse().expect("a literal address")
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.serving.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// One peer: its key, its one socket, and its connection to the relay.
struct Peer {
    key: Arc<DeviceKey>,
    routes: Arc<Routes>,
    endpoint: quinn::Endpoint,
    relay: Arc<RelayClient>,
}

impl Peer {
    /// A peer that only reaches somebody, so its endpoint carries no server
    /// configuration and discards an incoming Initial rather than answering it.
    fn client(state: &std::path::Path, relay: &RelayAddress) -> Self {
        Self::open(state, relay, None, Role::Client, (Ipv4Addr::LOCALHOST, 0).into())
    }

    /// A peer that can be reached, and takes only the devices in `clients`.
    fn host(state: &std::path::Path, relay: &RelayAddress, clients: Arc<PairedClients>) -> Self {
        Self::listening(state, relay, clients, (Ipv4Addr::LOCALHOST, 0).into())
    }

    /// The same, bound where the caller says.
    fn listening(
        state: &std::path::Path,
        relay: &RelayAddress,
        clients: Arc<PairedClients>,
        bind: SocketAddr,
    ) -> Self {
        let key = DeviceKey::load_or_create(state).expect("a device key");
        let server = server_config(&key, clients, tuning()).expect("a server configuration");
        Self::open(state, relay, Some(server), Role::Host, bind)
    }

    fn open(
        state: &std::path::Path,
        relay: &RelayAddress,
        server: Option<quinn::ServerConfig>,
        role: Role,
        bind: SocketAddr,
    ) -> Self {
        let key = Arc::new(DeviceKey::load_or_create(state).expect("a device key"));
        let routes = Arc::new(Routes::new());
        let (endpoint, socket) = endpoint(bind, server, &routes).expect("an endpoint");
        let client = RelayClient::open(
            relay.clone(),
            role,
            Arc::clone(&key),
            endpoint.clone(),
            Arc::clone(&socket),
        );
        Self {
            key,
            routes,
            endpoint,
            relay: Arc::new(client),
        }
    }

    fn device(&self) -> DeviceId {
        self.key.id()
    }

    fn at(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("a bound address")
    }
}

fn tuning() -> Tuning {
    Tuning {
        video_buffer: video_send_buffer(CodedSize::new(1920, 1088).expect("a codeable size")),
        initial_mtu: tachyx_transport::RELAYED_MTU,
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        viewport: Size::new(2400, 1080).expect("a size"),
        max_decode: Size::new(1920, 1088).expect("a size"),
        max_decode_fps: std::num::NonZeroU16::new(60).expect("nonzero"),
        requested_fps: std::num::NonZeroU16::new(60).expect("nonzero"),
        bitrate_ceiling: std::num::NonZeroU32::new(20_000_000).expect("nonzero"),
        decode: Codecs::of(Codec::H264),
    }
}

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tachyx-relay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A paired list holding one device.
fn paired_with(state: &std::path::Path, device: DeviceId) -> Arc<PairedClients> {
    let clients = Arc::new(PairedClients::open(state.join("clients.toml")).expect("a paired list"));
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

/// Waits until `count` hosts are reachable through `relay`.
///
/// A host is reachable when the relay has read its `Register`, and `Observed`
/// arrives before that, because the relay greets before it reads anything. In a
/// running system a host registered hours before any client asks for it; in a
/// test the two are microseconds apart and the order is whatever the scheduler
/// picked.
async fn registered(relay: &Loopback, count: usize) {
    settles(|| relay.relay.counts().registered >= count).await;
}

/// Waits until the relay has caught up with something that was asked of it.
///
/// A peer's request is a write on a stream, so it lands after the call that made
/// it has returned. Nothing in the protocol acknowledges one.
async fn settles(mut done: impl FnMut() -> bool) {
    let waiting = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(PATIENCE, waiting)
        .await
        .expect("the relay caught up");
}

fn seconds_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_pair_through_a_mailbox_the_relay_cannot_read() {
    let relay = Loopback::open("mailbox");
    let state = scratch("mailbox");
    let host = Peer::client(&state.join("host"), &relay.address);
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("the host reaches the relay");
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("the client reaches the relay");

    // The host opens a mailbox and shows the code.
    let nameplate = host.relay.allocate().await.expect("a mailbox");
    let (host_side, flight_one) =
        HostPairing::start(PairingCodes::fresh(nameplate)).expect("a pairing starts");
    assert_eq!(host_side.nameplate(), nameplate);
    // The code is read off the live exchange, because there is nothing else to
    // read it off: one draw opens one exchange and the draw is consumed.
    let shown = host_side.typed_text();
    host.relay.mail(flight_one).expect("the flight goes out");

    // Somebody types what is on the host's screen.
    let typed = tachyx_crypto::PairingCode::parse_typed(&shown).expect("a code");
    client
        .relay
        .join(typed.nameplate)
        .expect("the mailbox opens");
    let flight_one = client
        .relay
        .next_mail(PATIENCE)
        .await
        .expect("the host's flight arrives");
    let announced = ClientRecord {
        device: client.device(),
        name: "a phone".to_owned(),
    };
    let (client_side, flight_two) =
        ClientPairing::answer(&typed, &flight_one, None, &announced).expect("the code opens");
    client.relay.mail(flight_two).expect("the flight goes out");

    let flight_two = host
        .relay
        .next_mail(PATIENCE)
        .await
        .expect("the client's flight arrives");
    let record = HostRecord {
        device: host.device(),
        name: "a desk".to_owned(),
        home_relay: Some(relay.address.clone()),
        endpoints: vec![host.at()],
        permissions: Permissions::default(),
    };
    let (learned_client, flight_three, host_words) = host_side
        .finish(&flight_two, &record)
        .expect("the code was right");
    host.relay.mail(flight_three).expect("the flight goes out");
    host.relay.release().expect("the mailbox closes");

    let flight_three = client
        .relay
        .next_mail(PATIENCE)
        .await
        .expect("the host's record arrives");
    let (learned_host, client_words) = client_side.finish(&flight_three).expect("the record opens");

    println!(
        "\n  mailbox            {nameplate}\n  \
           the host learned   {} ({})\n  \
           the client learned {} ({})\n  \
           short auth         {host_words}\n",
        learned_client.device, learned_client.name, learned_host.device, learned_host.name,
    );
    assert_eq!(learned_client.device, client.device());
    assert_eq!(learned_host.device, host.device());
    assert_eq!(
        host_words, client_words,
        "both screens show the same words, or the relay was in the middle"
    );
    settles(|| relay.relay.counts().pairing == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spliced_session_carries_a_real_session_end_to_end() {
    let relay = Loopback::open("splice");
    let state = scratch("splice");
    let client_key = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, Arc::clone(&clients));
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("the host registers");
    registered(&relay, 1).await;
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("the client reaches the relay");

    // The splice is asked for directly. On loopback a punch always works, and
    // what is being checked here is the path that opens when it does not.
    let listening = Host::on_endpoint(host.endpoint.clone(), tuning());
    let draining = Arc::clone(&host.relay);
    let host_lane = tokio::spawn(async move { draining.next_lane().await });

    let lane: Lane = tokio::time::timeout(PATIENCE, client.relay.splice(host.device()))
        .await
        .expect("the relay answers")
        .expect("a lane opens");
    let host_lane = tokio::time::timeout(PATIENCE, host_lane)
        .await
        .expect("the host is told about the lane")
        .expect("the task did not panic")
        .expect("a lane");
    assert_eq!(host_lane.peer, client.device());

    let dialling = Client::on_endpoint(client.endpoint.clone(), Arc::clone(&client.key), tuning());
    let hello = ClientHello::new(capabilities());
    let (accepted, opened) = tokio::join!(
        tokio::time::timeout(PATIENCE, listening.accept()),
        tokio::time::timeout(
            PATIENCE,
            dialling.connect(lane.known_as, host.device(), &hello)
        )
    );
    let mut accepted: HostSession = accepted
        .expect("the host accepts inside the deadline")
        .expect("the host accepts");
    let mut opened: ClientSession = opened
        .expect("the client connects inside the deadline")
        .expect("the client connects");

    assert_eq!(accepted.peer, client.device());
    assert_eq!(opened.peer, host.device());
    assert_eq!(
        opened.connection.remote_address(),
        lane.known_as,
        "quinn was told a virtual address and never learns it is relayed"
    );

    // A real control message and a real video datagram, through nine bytes of
    // relay header each way.
    let configured = HostMessage::Stream(StreamConfig {
        codec: Codec::H264,
        coded: CodedSize::new(1920, 1088).expect("a codeable size"),
        visible: Size::new(1920, 1080).expect("a size"),
        display_aspect: tachyx_proto::control::Aspect::new(16, 9).expect("a ratio"),
        framerate: std::num::NonZeroU16::new(60).expect("nonzero"),
        bitrate: std::num::NonZeroU32::new(8_000_000).expect("nonzero"),
    });
    accepted
        .to_client
        .send(&configured)
        .await
        .expect("a live stream");
    let read = tokio::time::timeout(PATIENCE, opened.from_host.recv::<HostMessage>())
        .await
        .expect("the message arrives")
        .expect("the message decodes");
    assert_eq!(read, configured);

    let request = ClientMessage::KeyframeRequest { after: None };
    opened.to_host.send(&request).await.expect("a live stream");
    let read = tokio::time::timeout(PATIENCE, accepted.from_client.recv::<ClientMessage>())
        .await
        .expect("the message arrives")
        .expect("the message decodes");
    assert_eq!(read, request);

    let picture = vec![7u8; 900];
    accepted
        .connection
        .send_datagram(picture.clone().into())
        .expect("a datagram fits the relayed path");
    let datagram = tokio::time::timeout(PATIENCE, opened.connection.read_datagram())
        .await
        .expect("the datagram arrives")
        .expect("the connection is live");
    assert_eq!(&datagram[..], &picture[..]);

    println!(
        "\n  relay              {}\n  \
           quinn sees         {}\n  \
           datagram limit     {:?} bytes through nine of relay header\n  \
           round trip         {:?}\n",
        relay.address,
        opened.connection.remote_address(),
        opened.connection.max_datagram_size(),
        opened.connection.rtt(),
    );
    assert_eq!(relay.relay.counts().lane_ends, 2, "one splice is two ends");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_lane_token_is_dropped_without_an_answer() {
    let relay = Loopback::open("forged");
    let state = scratch("forged");
    let client_key = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, clients);
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("registered");
    registered(&relay, 1).await;
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("connected");

    let draining = Arc::clone(&host.relay);
    let held = tokio::spawn(async move { draining.next_lane().await });
    let lane = tokio::time::timeout(PATIENCE, client.relay.splice(host.device()))
        .await
        .expect("the relay answers")
        .expect("a lane opens");
    let _ = tokio::time::timeout(PATIENCE, held).await;

    // A stranger's socket, which holds no lane at all.
    let stranger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a socket");
    stranger
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("a deadline");

    let mut frame = Vec::new();
    for forged in [
        tachyx_relay::LaneToken::from_bytes([0u8; 8]),
        tachyx_relay::LaneToken::from_bytes([0xff; 8]),
    ] {
        splice_frame(forged, &[9u8; 64], &mut frame);
        stranger
            .send_to(&frame, relay.at())
            .expect("the send lands");
    }
    let mut back = [0u8; 2048];
    assert!(
        stranger.recv_from(&mut back).is_err(),
        "a token the relay does not hold is dropped without an answer, or there is \
         something to probe against"
    );

    // The lane the two peers really hold is untouched by any of that.
    let dialling = Client::on_endpoint(client.endpoint.clone(), Arc::clone(&client.key), tuning());
    let listening = Host::on_endpoint(host.endpoint.clone(), tuning());
    let hello = ClientHello::new(capabilities());
    let (accepted, opened) = tokio::join!(
        tokio::time::timeout(PATIENCE, listening.accept()),
        tokio::time::timeout(
            PATIENCE,
            dialling.connect(lane.known_as, host.device(), &hello)
        )
    );
    assert!(accepted.expect("in time").is_ok());
    assert!(opened.expect("in time").is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_token_used_from_another_address_reaches_nobody() {
    let relay = Loopback::open("replayed");
    let state = scratch("replayed");
    let client_key = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, clients);
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("registered");
    registered(&relay, 1).await;
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("connected");

    let draining = Arc::clone(&host.relay);
    let held = tokio::spawn(async move { draining.next_lane().await });
    let lane = tokio::time::timeout(PATIENCE, client.relay.splice(host.device()))
        .await
        .expect("the relay answers")
        .expect("a lane opens");
    let _ = tokio::time::timeout(PATIENCE, held).await;

    // The client's own token, read off its route table, replayed from a socket
    // that is not the client's.
    let tachyx_relay::Route::Relayed { token, .. } = client
        .routes
        .route(lane.known_as)
        .expect("the lane is routed")
    else {
        panic!("a spliced peer is routed through a relay")
    };

    let elsewhere = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a socket");
    elsewhere
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("a deadline");
    let mut frame = Vec::new();
    splice_frame(token, &[SPLICE_TAG; 200], &mut frame);
    elsewhere
        .send_to(&frame, relay.at())
        .expect("the send lands");
    let mut back = [0u8; 2048];
    assert!(
        elsewhere.recv_from(&mut back).is_err(),
        "a token overheard on the path must not be replayable from anywhere else"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offer_for_one_host_is_refused_by_another() {
    let relay = Loopback::open("misaddressed");
    let state = scratch("misaddressed");
    let client_key =
        Arc::new(DeviceKey::load_or_create(&state.join("client")).expect("a client key"));
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, Arc::clone(&clients));
    let elsewhere = DeviceKey::load_or_create(&state.join("elsewhere")).expect("a third key");
    let stranger = DeviceKey::load_or_create(&state.join("stranger")).expect("a fourth key");

    let answering = HostRendezvous::new(
        Arc::clone(&host.key),
        clients,
        Arc::clone(&host.relay),
        host.endpoint.clone(),
        Arc::new(Arrivals::new()),
        tuning(),
    );
    let victim: SocketAddr = "203.0.113.5:47989".parse().expect("an address");

    // Collected for another host, replayed at this one.
    let misaddressed =
        Offer::sign(&client_key, elsewhere.id(), vec![victim], seconds_now()).expect("an offer");
    assert!(
        matches!(
            answering.answer(&misaddressed, &[]).await,
            Err(RelayError::NotForUs)
        ),
        "`to` is inside the signature so an offer cannot be moved between hosts"
    );

    // Signed properly, for this host, by a device it has never paired with.
    let unpaired =
        Offer::sign(&stranger, host.device(), vec![victim], seconds_now()).expect("an offer");
    assert!(matches!(
        answering.answer(&unpaired, &[]).await,
        Err(RelayError::NotForUs)
    ));

    // The signature itself, over an address somebody else wrote in.
    let mut tampered =
        Offer::sign(&client_key, host.device(), vec![], seconds_now()).expect("an offer");
    tampered.candidates.push(victim);
    assert!(
        answering.answer(&tampered, &[]).await.is_err(),
        "the candidate list is what a reflector would rewrite"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_punch_on_one_machine_finds_the_direct_path_and_the_relay_carries_nothing() {
    let relay = Loopback::open("punch");
    let state = scratch("punch");
    let client_key =
        Arc::new(DeviceKey::load_or_create(&state.join("client")).expect("a client key"));
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, Arc::clone(&clients));
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("registered");
    registered(&relay, 1).await;

    let arrivals = Arc::new(Arrivals::new());
    let answering = HostRendezvous::new(
        Arc::clone(&host.key),
        clients,
        Arc::clone(&host.relay),
        host.endpoint.clone(),
        Arc::clone(&arrivals),
        tuning(),
    );
    let serving = tokio::spawn(async move { answering.serve(&[]).await });

    let listening = Host::on_endpoint(host.endpoint.clone(), tuning());
    let noticing = Arc::clone(&arrivals);
    let accepting = tokio::spawn(async move {
        let session = listening.accept().await.expect("the host accepts");
        noticing.note(session.peer);
        session
    });

    let reaching = ClientRendezvous::new(
        Arc::clone(&client.key),
        Arc::clone(&client.relay),
        Arc::clone(&client.routes),
        Client::on_endpoint(client.endpoint.clone(), Arc::clone(&client.key), tuning()),
    );
    let hello = ClientHello::new(capabilities());
    let (session, path) = tokio::time::timeout(PATIENCE, reaching.open(host.device(), &[], &hello))
        .await
        .expect("the rendezvous finishes")
        .expect("a session opens");
    let accepted = tokio::time::timeout(PATIENCE, accepting)
        .await
        .expect("the host accepted")
        .expect("the task did not panic");

    println!(
        "\n  path               {path:?}\n  \
           quinn sees          {}\n  \
           relay lane ends     {}\n",
        session.connection.remote_address(),
        relay.relay.counts().lane_ends,
    );
    assert_eq!(path, Path::Direct, "two peers on one machine punch through");
    assert_eq!(accepted.peer, client.device());
    assert_eq!(session.peer, host.device());
    assert_eq!(
        relay.relay.counts().lane_ends,
        0,
        "a punch that worked costs the relay no lane at all"
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_restarts_has_forgotten_everything_and_the_host_comes_back() {
    let state = scratch("restart");
    let key = DeviceKey::load_or_create(&state.join("relay")).expect("a relay key");
    let first = Relay::bind((Ipv4Addr::LOCALHOST, 0).into(), &key).expect("a bound relay");
    let at = first.local_address().expect("a bound address");
    let address = first.advertised().expect("a bound address");
    let running = Arc::new(first);
    let serving = tokio::spawn({
        let running = Arc::clone(&running);
        async move { running.run().await }
    });

    let clients =
        Arc::new(PairedClients::open(state.join("clients.toml")).expect("a paired list"));
    let host = Peer::host(&state.join("host"), &address, clients);
    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("the host reaches the relay");
    settles(|| running.counts().registered == 1).await;

    // The relay goes away, taking everything it held with it.
    running.shutdown().await;
    serving.abort();
    drop(running);

    // It comes back at the same address, with the same key and no memory. The
    // port is released by quinn's own driver task, which needs a moment after
    // the endpoint closes.
    let mut bound = None;
    for _ in 0..200 {
        match Relay::bind(at, &key) {
            Ok(relay) => {
                bound = Some(relay);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let second = Arc::new(bound.expect("the port comes free"));
    assert_eq!(
        second.counts().registered,
        0,
        "a restarted relay has forgotten every host"
    );
    let serving = tokio::spawn({
        let second = Arc::clone(&second);
        async move { second.run().await }
    });

    let waiting = async {
        while second.counts().registered != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), waiting)
        .await
        .expect("a host whose relay connection is gone must come back on its own");
    println!(
        "\n  relay              {address}\n  \
           after a restart    the host registered again with nothing asking it to\n"
    );
    serving.abort();
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dual_stack_peer_is_spliced_through_an_ipv4_relay() {
    // A dual-stack socket takes an IPv4 destination only as `::ffff:a.b.c.d`.
    // quinn writes the destinations it picks itself that way; a destination that
    // came out of the routing table is not one of those, and a bare IPv4
    // sockaddr on this socket is refused by the kernel outright.
    let relay = Loopback::open("dualstack");
    let state = scratch("dualstack");
    let client_key = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = paired_with(&state, client_key.id());
    let dual: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, 0).into();
    let host = Peer::listening(&state.join("host"), &relay.address, clients, dual);
    let client = Peer::open(
        &state.join("client"),
        &relay.address,
        None,
        Role::Client,
        dual,
    );
    assert!(host.at().is_ipv6() && client.at().is_ipv6());
    assert!(relay.at().is_ipv4(), "the relay is reached over IPv4");

    host.relay
        .wait_observed(PATIENCE)
        .await
        .expect("a dual-stack host reaches an IPv4 relay");
    registered(&relay, 1).await;
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("a dual-stack client reaches an IPv4 relay");

    let draining = Arc::clone(&host.relay);
    let held = tokio::spawn(async move { draining.next_lane().await });
    let lane = tokio::time::timeout(PATIENCE, client.relay.splice(host.device()))
        .await
        .expect("the relay answers")
        .expect("a lane opens");
    let _ = tokio::time::timeout(PATIENCE, held).await;

    let listening = Host::on_endpoint(host.endpoint.clone(), tuning());
    let dialling =
        Client::on_endpoint(client.endpoint.clone(), Arc::clone(&client.key), tuning());
    let hello = ClientHello::new(capabilities());
    let (accepted, opened) = tokio::join!(
        tokio::time::timeout(PATIENCE, listening.accept()),
        tokio::time::timeout(
            PATIENCE,
            dialling.connect(lane.known_as, host.device(), &hello)
        )
    );
    let accepted = accepted.expect("in time").expect("the host accepts");
    let opened = opened.expect("in time").expect("the client connects");
    println!(
        "\n  relay              {}\n  host bound         {}\n  client bound       {}\n",
        relay.address,
        host.at(),
        client.at()
    );
    assert_eq!(accepted.peer, client.device());
    assert_eq!(opened.peer, host.device());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relayed_session_moved_onto_the_wire_keeps_the_connection_it_had() {
    // The upgrade is not switched on: one congestion controller cannot hold the
    // window and the round trip of what have become two paths, and that is what
    // is missing rather than this. The mechanism is here, and this is what says
    // it works: the route under a live session changes and quinn never learns.
    let relay = Loopback::open("upgrade");
    let state = scratch("upgrade");
    let client_key = DeviceKey::load_or_create(&state.join("client")).expect("a client key");
    let clients = paired_with(&state, client_key.id());
    let host = Peer::host(&state.join("host"), &relay.address, clients);
    let client = Peer::client(&state.join("client"), &relay.address);
    host.relay.wait_observed(PATIENCE).await.expect("registered");
    registered(&relay, 1).await;
    client
        .relay
        .wait_observed(PATIENCE)
        .await
        .expect("connected");

    let draining = Arc::clone(&host.relay);
    let held = tokio::spawn(async move { draining.next_lane().await });
    let lane = tokio::time::timeout(PATIENCE, client.relay.splice(host.device()))
        .await
        .expect("the relay answers")
        .expect("a lane opens");
    let host_lane = tokio::time::timeout(PATIENCE, held)
        .await
        .expect("in time")
        .expect("the task did not panic")
        .expect("a lane");

    let listening = Host::on_endpoint(host.endpoint.clone(), tuning());
    let dialling =
        Client::on_endpoint(client.endpoint.clone(), Arc::clone(&client.key), tuning());
    let hello = ClientHello::new(capabilities());
    let (accepted, opened) = tokio::join!(
        tokio::time::timeout(PATIENCE, listening.accept()),
        tokio::time::timeout(
            PATIENCE,
            dialling.connect(lane.known_as, host.device(), &hello)
        )
    );
    let mut accepted = accepted.expect("in time").expect("the host accepts");
    let mut opened = opened.expect("in time").expect("the client connects");
    let was = opened.connection.remote_address();
    let relayed_rtt = opened.connection.rtt();
    assert!(
        relay.relay.counts().forwarded > 0,
        "the handshake went through the relay"
    );

    // Both ends swap the route under the live session, in one write each.
    client
        .routes
        .point(lane.known_as, tachyx_relay::Route::Direct(host.at()));
    host.routes
        .point(host_lane.known_as, tachyx_relay::Route::Direct(client.at()));

    // Long enough for whatever was already on its way to the relay to land, so
    // what is counted below is only what the move should have stopped.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let relayed_bytes = relay.relay.counts().forwarded;

    // The same connection carries the next message, over the wire this time.
    let request = ClientMessage::KeyframeRequest { after: None };
    opened.to_host.send(&request).await.expect("a live stream");
    let read = tokio::time::timeout(PATIENCE, accepted.from_client.recv::<ClientMessage>())
        .await
        .expect("the message arrives on the direct path")
        .expect("the message decodes");
    assert_eq!(read, request);
    let picture = vec![3u8; 900];
    accepted
        .connection
        .send_datagram(picture.clone().into())
        .expect("a datagram fits");
    let datagram = tokio::time::timeout(PATIENCE, opened.connection.read_datagram())
        .await
        .expect("the datagram arrives")
        .expect("the connection is live");
    assert_eq!(&datagram[..], &picture[..]);

    let after = relay.relay.counts().forwarded;
    println!(
        "\n  quinn saw          {was} before and {} after\n  \
           round trip         {relayed_rtt:?} relayed, {:?} direct\n  \
           relay carried      {relayed_bytes} bytes, and {} more after the move\n",
        opened.connection.remote_address(),
        opened.connection.rtt(),
        after - relayed_bytes,
    );
    assert_eq!(
        opened.connection.remote_address(),
        was,
        "the session keeps the address it has always had, so nothing migrates"
    );
    assert_eq!(
        after, relayed_bytes,
        "everything after the move went over the wire, and the relay carried none of it"
    );
    assert_eq!(accepted.peer, client.device());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_connections_from_one_prefix_stops_being_admitted() {
    // A public relay is a machine strangers can reach. The budget is per IPv6
    // `/64` rather than per address, because a single subscriber is handed a
    // whole one, and on loopback every peer here is one subscriber.
    let relay = Loopback::open("burst");
    let state = scratch("burst");
    let mut peers = Vec::new();
    for index in 0..24 {
        peers.push(Peer::client(
            &state.join(format!("peer{index}")),
            &relay.address,
        ));
    }
    // Awaited together, because what is being measured is which connections came
    // up at once rather than which came up eventually: the budget refills.
    let waiting: Vec<_> = peers
        .iter()
        .map(|peer| {
            let relay = Arc::clone(&peer.relay);
            tokio::spawn(async move { relay.wait_observed(Duration::from_millis(800)).await })
        })
        .collect();
    let mut admitted = 0;
    for handle in waiting {
        if handle.await.expect("the task did not panic").is_ok() {
            admitted += 1;
        }
    }
    println!("\n  offered            {} connections\n  admitted           {admitted}\n", peers.len());
    assert!(
        admitted > 0,
        "the first connections from a prefix are ordinary and must be served"
    );
    assert!(
        admitted < peers.len(),
        "a prefix that opens {} connections at once is over its budget and the \
         rest are not admitted",
        peers.len()
    );
}
