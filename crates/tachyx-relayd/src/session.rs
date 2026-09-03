//! One control connection, from the handshake to whatever ends it.
//!
//! The relay takes the client certificate's key as the peer's identity and
//! **authorises nothing**: it needs a stable name to route by, not a decision
//! about who may talk to whom. That decision belongs to the host, which makes it
//! in its own TLS handshake, and to nobody else.

use std::sync::Arc;
use std::time::Duration;

use quinn::Connection;
use tachyx_crypto::DeviceId;
use tachyx_relay::proto::{
    Hello, PeerHello, PeerMessage, Refusal, RelayHello, RelayMessage, MAX_MESSAGE,
};
use tachyx_transport::{peer_device, ControlReader, ControlWriter};
use tokio::sync::mpsc;

use crate::limits::Messages;
use crate::splice::{canonical, Lanes};
use crate::state::{Outbound, Registered, Registry};
use crate::RelaydError;

/// How often a connection's address is compared with the one it had.
///
/// A NAT rebinding is what a change looks like from here, and five seconds is
/// far below the time any peer would notice one.
const ADDRESS_TICK: Duration = Duration::from_secs(5);

/// Everything a connection task needs.
#[derive(Debug)]
pub struct Serving {
    /// Who is here, which mailboxes are open, and whose lanes are whose.
    pub registry: Arc<Registry>,
    /// The splices this relay is forwarding.
    pub lanes: Arc<Lanes>,
}

/// Serves one peer until its connection ends.
///
/// # Errors
/// If the peer presents nothing to be named by, speaks another version of this
/// protocol, or the connection fails.
pub async fn serve(serving: &Serving, connection: Connection) -> Result<(), RelaydError> {
    let device = peer_device(&connection).ok_or(RelaydError::Anonymous)?;
    let id = serving.registry.number();
    let mut at = canonical(connection.remote_address());
    tracing::info!(%device, %at, "a peer connected");

    let outcome = talk(serving, &connection, device, id, &mut at).await;

    for token in serving.registry.forget(device, id) {
        if let Some(traffic) = serving.lanes.traffic(token) {
            tracing::info!(
                %device,
                received = traffic.received,
                sent = traffic.sent,
                "a spliced lane closed"
            );
        }
        serving.lanes.close(token);
    }
    tracing::info!(%device, %at, "a peer disconnected");
    outcome
}

/// The hellos, then messages until something ends it.
async fn talk(
    serving: &Serving,
    connection: &Connection,
    device: DeviceId,
    id: u64,
    at: &mut std::net::SocketAddr,
) -> Result<(), RelaydError> {
    // The peer opens the stream, so the relay waits for it rather than racing to
    // open one of its own.
    let (send, recv) = connection.accept_bi().await?;
    let mut writer = ControlWriter::new(send);
    let mut reader = ControlReader::new(recv);
    // The relay greets before it reads, so a peer this build cannot talk to is
    // told which version the relay speaks rather than left with a connection
    // that closed for no stated reason.
    writer.send(&RelayHello::new()).await?;
    let _: PeerHello = PeerHello::decode(reader.bytes().await?)?;

    let (out, mut queued) = mpsc::unbounded_channel();
    serving.registry.connect(
        device,
        Registered {
            id,
            at: *at,
            out: out.clone(),
        },
    );
    // Sent unprompted as soon as the connection is up. This is the whole of the
    // reflexive address machinery: there is no STUN server, and no third-party
    // STUN server.
    out.send(RelayMessage::Observed { address: *at })
        .map_err(|_| RelaydError::Gone)?;

    let budget = Messages::new();
    let mut tick = tokio::time::interval(ADDRESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            outgoing = queued.recv() => match outgoing {
                Some(message) => writer.send(&message).await?,
                None => return Ok(()),
            },
            incoming = reader.bytes() => {
                let bytes = incoming?;
                if bytes.len() > MAX_MESSAGE {
                    return Err(RelaydError::Malformed);
                }
                let message: PeerMessage =
                    postcard::from_bytes(bytes).map_err(|_| RelaydError::Malformed)?;
                if !budget.admits() {
                    let _ = out.send(RelayMessage::Refused { what: Refusal::RateLimited });
                    continue;
                }
                if let Err(what) = act(serving, device, id, *at, &out, message) {
                    let _ = out.send(RelayMessage::Refused { what });
                }
            }
            _ = tick.tick() => {
                let now = canonical(connection.remote_address());
                if now != *at {
                    // What a NAT rebinding looks like from here. The peer is
                    // told, and its lanes move with it, because a lane forwards
                    // to the address the control connection is at.
                    tracing::info!(%device, was = %at, now = %now, "a peer moved");
                    *at = now;
                    for token in serving.registry.moved(device, id, now) {
                        serving.lanes.repoint(token, now);
                    }
                    let _ = out.send(RelayMessage::Observed { address: now });
                }
            }
        }
    }
}

/// Does one thing a peer asked for.
fn act(
    serving: &Serving,
    device: DeviceId,
    id: u64,
    at: std::net::SocketAddr,
    out: &Outbound,
    message: PeerMessage,
) -> Result<(), Refusal> {
    match message {
        PeerMessage::Register => {
            serving.registry.register(
                device,
                Registered {
                    id,
                    at,
                    out: out.clone(),
                },
            );
            tracing::info!(%device, %at, "a host is reachable through this relay");
            Ok(())
        }
        PeerMessage::Allocate => {
            let nameplate = serving.registry.allocate(id, out.clone())?;
            out.send(RelayMessage::Nameplate { nameplate })
                .map_err(|_| Refusal::NotInMailbox)
        }
        PeerMessage::Join { nameplate } => serving.registry.join(nameplate, id, out.clone()),
        PeerMessage::Mail { body } => serving.registry.mail(id, body),
        PeerMessage::Release => {
            serving.registry.release(id);
            Ok(())
        }
        // The offer is forwarded and not read. The relay could not verify it
        // usefully if it tried: it is signed for the peer at the far end, which
        // checks it against its own paired list before it emits one packet.
        PeerMessage::Connect { offer } => serving
            .registry
            .send_to(offer.to, RelayMessage::Offered { offer }),
        // Back along a conversation that is already open, so it is routed by
        // the connection rather than by presence. A client does not register.
        PeerMessage::Answer { offer } => serving
            .registry
            .reply_to(offer.to, RelayMessage::Answered { offer }),
        PeerMessage::Splice { peer } => {
            let other = serving.registry.lookup(peer).ok_or(Refusal::UnknownPeer)?;
            let (mine, theirs) = serving.lanes.open(at, other.at);
            serving.registry.attach_lane(id, mine);
            serving.registry.attach_lane(other.id, theirs);
            // The far end first: it has to have its token, and have proved its
            // address with it, before anything the asking peer sends can be
            // forwarded.
            other
                .out
                .send(RelayMessage::Lane {
                    token: theirs,
                    peer: device,
                })
                .map_err(|_| Refusal::UnknownPeer)?;
            out.send(RelayMessage::Lane { token: mine, peer })
                .map_err(|_| Refusal::UnknownPeer)
        }
    }
}
