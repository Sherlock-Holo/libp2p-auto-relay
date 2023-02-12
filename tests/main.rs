use std::future::ready;
use std::io::{self, ErrorKind};
use std::time::Duration;

use futures_util::StreamExt;
use libp2p::ping;
use libp2p_auto_relay::combine::CombineTransport;
use libp2p_auto_relay::{endpoint, relay};
use libp2p_core::identity::Keypair;
use libp2p_core::multiaddr::Protocol;
use libp2p_core::muxing::StreamMuxerBox;
use libp2p_core::transport::{Boxed, MemoryTransport};
use libp2p_core::upgrade::Version;
use libp2p_core::{Multiaddr, PeerId, Transport};
use libp2p_noise::NoiseAuthenticated;
use libp2p_swarm::{keep_alive, NetworkBehaviour, Swarm, SwarmEvent};
use libp2p_yamux::YamuxConfig;
use tokio::time;
use tracing::level_filters::LevelFilter;
use tracing::{debug, subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, Registry};

#[derive(Debug, NetworkBehaviour, Default)]
struct RelayBehaviour {
    relay: relay::Behaviour,
}

#[derive(NetworkBehaviour)]
struct ServerBehaviour {
    client: endpoint::Behaviour,
    keepalive: keep_alive::Behaviour,
    ping: ping::Behaviour,
}

#[tokio::test]
async fn test() {
    init_log();
    run().await
}

async fn run() {
    let client = Keypair::generate_ed25519();
    let relay = Keypair::generate_ed25519();
    let server = Keypair::generate_ed25519();
    let relay_addr = Multiaddr::empty()
        .with(Protocol::Memory(2))
        .with(Protocol::P2p(relay.public().to_peer_id().into()));

    let (client_transport, client_behaviour) = build_client_transport(
        &client,
        server.public().to_peer_id(),
        relay.public().to_peer_id(),
        relay_addr.clone(),
    );

    let relay_transport = build_relay_transport(&relay);
    let (server_transport, server_behaviour) =
        build_server_transport(&server, relay.public().to_peer_id(), relay_addr.clone());

    run_relay(relay_transport, relay_addr.clone(), relay);
    run_server(server_transport, server.clone(), server_behaviour);

    time::sleep(Duration::from_secs(2)).await;

    run_client(client_transport, client, server, client_behaviour).await;
}

async fn run_client(
    client_transport: Boxed<(PeerId, StreamMuxerBox)>,
    client: Keypair,
    server: Keypair,
    client_behaviour: endpoint::Behaviour,
) {
    let peer_id = client.public().to_peer_id();
    let server_peer_id = server.public().to_peer_id();
    let mut swarm = Swarm::with_tokio_executor(
        client_transport,
        ServerBehaviour {
            client: client_behaviour,
            keepalive: Default::default(),
            ping: Default::default(),
        },
        peer_id,
    );

    swarm
        .dial(
            Multiaddr::empty()
                .with(Protocol::Memory(3))
                .with(Protocol::P2p(server_peer_id.into())),
        )
        .unwrap();

    while let Some(event) = swarm.next().await {
        match event {
            SwarmEvent::Behaviour(ServerBehaviourEvent::Ping(event)) if event.result.is_ok() => {
                debug!("endpoint ping event {event:?}");

                break;
            }

            _ => {
                debug!("endpoint event {event:?}");
            }
        }
    }
}

fn run_server(
    server_transport: Boxed<(PeerId, StreamMuxerBox)>,
    server: Keypair,
    server_behaviour: endpoint::Behaviour,
) {
    let peer_id = server.public().to_peer_id();
    let mut swarm = Swarm::with_tokio_executor(
        server_transport,
        ServerBehaviour {
            client: server_behaviour,
            keepalive: Default::default(),
            ping: Default::default(),
        },
        peer_id,
    );

    swarm
        .listen_on(
            Multiaddr::empty()
                .with(Protocol::Memory(3))
                .with(Protocol::P2p(peer_id.into())),
        )
        .unwrap();

    tokio::spawn(async move {
        while let Some(event) = swarm.next().await {
            if let SwarmEvent::Behaviour(ServerBehaviourEvent::Ping(event)) = event {
                debug!("relay ping {event:?}");
            } else {
                debug!("relay other {event:?}");
            }
        }
    });
}

fn run_relay(
    relay_transport: Boxed<(PeerId, StreamMuxerBox)>,
    relay_addr: Multiaddr,
    relay_keypair: Keypair,
) {
    let mut swarm = Swarm::with_tokio_executor(
        relay_transport,
        RelayBehaviour::default(),
        relay_keypair.public().to_peer_id(),
    );

    swarm.listen_on(relay_addr).unwrap();

    tokio::spawn(async move {
        while let Some(event) = swarm.next().await {
            debug!("relay {event:?}");
        }
    });
}

fn build_client_transport(
    client: &Keypair,
    server_peer_id: PeerId,
    relay_peer_id: PeerId,
    relay_addr: Multiaddr,
) -> (Boxed<(PeerId, StreamMuxerBox)>, endpoint::Behaviour) {
    let (relay_transport, client_behaviour) =
        endpoint::Transport::new(client.public().to_peer_id(), relay_addr, relay_peer_id);

    let mem_transport = MemoryTransport::new().and_then(move |conn, endpoint| {
        if PeerId::try_from_multiaddr(endpoint.get_remote_address()).unwrap() == server_peer_id {
            ready(Err(io::Error::new(
                ErrorKind::Other,
                "relay can't connect directly",
            )))
        } else {
            ready(Ok(conn))
        }
    });

    let transport = CombineTransport::new(mem_transport, relay_transport)
        .upgrade(Version::V1)
        .authenticate(NoiseAuthenticated::xx(client).unwrap())
        .multiplex(YamuxConfig::default())
        .boxed();

    (transport, client_behaviour)
}

fn build_relay_transport(relay: &Keypair) -> Boxed<(PeerId, StreamMuxerBox)> {
    MemoryTransport::new()
        .upgrade(Version::V1)
        .authenticate(NoiseAuthenticated::xx(relay).unwrap())
        .multiplex(YamuxConfig::default())
        .boxed()
}

fn build_server_transport(
    server: &Keypair,
    relay_peer_id: PeerId,
    relay_addr: Multiaddr,
) -> (Boxed<(PeerId, StreamMuxerBox)>, endpoint::Behaviour) {
    let (relay_transport, server_behaviour) =
        endpoint::Transport::new(server.public().to_peer_id(), relay_addr, relay_peer_id);

    let transport = CombineTransport::new(MemoryTransport::new(), relay_transport)
        .upgrade(Version::V1)
        .authenticate(NoiseAuthenticated::xx(server).unwrap())
        .multiplex(YamuxConfig::default())
        .boxed();

    (transport, server_behaviour)
}

fn init_log() {
    let layer = fmt::layer()
        .pretty()
        .with_target(true)
        .with_writer(io::stderr);

    let targets = Targets::new().with_default(LevelFilter::DEBUG);

    let layered = Registry::default()
        .with(targets)
        .with(layer)
        .with(LevelFilter::DEBUG);

    subscriber::set_global_default(layered).unwrap();
}
