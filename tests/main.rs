use std::collections::HashMap;
use std::future::ready;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::{future, FutureExt, StreamExt, TryFutureExt};
use libp2p::ping;
use libp2p_auto_relay::{endpoint, relay};
use libp2p_core::either::{EitherError, EitherFuture, EitherOutput};
use libp2p_core::identity::Keypair;
use libp2p_core::multiaddr::Protocol;
use libp2p_core::muxing::StreamMuxerBox;
use libp2p_core::transport::{Boxed, ListenerId, MemoryTransport, TransportError, TransportEvent};
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

#[derive(Debug, Clone)]
#[pin_project::pin_project]
pub struct CombineTransport<A, B> {
    #[pin]
    t1: A,
    #[pin]
    t2: B,
    listener_ids: HashMap<ListenerId, (ListenerId, ListenerId)>,
}

impl<A, B> CombineTransport<A, B> {
    pub fn new(a: A, b: B) -> CombineTransport<A, B> {
        CombineTransport {
            t1: a,
            t2: b,
            listener_ids: Default::default(),
        }
    }
}

type _DialResult<AO, BO, AE, BE> = Result<
    BoxFuture<'static, Result<EitherOutput<AO, BO>, EitherError<AE, BE>>>,
    TransportError<EitherError<AE, BE>>,
>;

impl<A, B> CombineTransport<A, B>
where
    A: Transport,
    B: Transport,
    A::Output: Send + 'static,
    B::Output: Send + 'static,
    A::Error: Send + 'static,
    B::Error: Send + 'static,
    A::Dial: Send + 'static,
    B::Dial: Send + 'static,
{
    fn _dial(
        &mut self,
        addr: Multiaddr,
        as_listener: bool,
    ) -> _DialResult<A::Output, B::Output, A::Error, B::Error> {
        if !as_listener {
            let dial1 = self.t1.dial(addr.clone());
            let dial2 = self.t2.dial(addr);

            match (dial1, dial2) {
                (Err(_), Err(err)) => Err(err.map(EitherError::B)),
                (Ok(dial), Err(_)) => {
                    let dial = dial
                        .map_err(EitherError::A)
                        .map_ok(EitherOutput::First)
                        .boxed();
                    Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
                }
                (Err(_), Ok(dial)) => {
                    let dial = dial
                        .map_err(EitherError::B)
                        .map_ok(EitherOutput::Second)
                        .boxed();
                    Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
                }
                (Ok(dial1), Ok(dial2)) => {
                    let dial1 = dial1
                        .map_err(EitherError::A)
                        .map_ok(EitherOutput::First)
                        .boxed();
                    let dial2 = dial2
                        .map_err(EitherError::B)
                        .map_ok(EitherOutput::Second)
                        .boxed();

                    Ok(future::select_ok([dial1, dial2])
                        .map_ok(|res| res.0)
                        .boxed())
                }
            }
        } else {
            let dial1 = self.t1.dial_as_listener(addr.clone());
            let dial2 = self.t2.dial_as_listener(addr);

            match (dial1, dial2) {
                (Err(_), Err(err)) => Err(err.map(EitherError::B)),
                (Ok(dial), Err(_)) => {
                    let dial = dial
                        .map_err(EitherError::A)
                        .map_ok(EitherOutput::First)
                        .boxed();
                    Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
                }
                (Err(_), Ok(dial)) => {
                    let dial = dial
                        .map_err(EitherError::B)
                        .map_ok(EitherOutput::Second)
                        .boxed();
                    Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
                }
                (Ok(dial1), Ok(dial2)) => {
                    let dial1 = dial1
                        .map_err(EitherError::A)
                        .map_ok(EitherOutput::First)
                        .boxed();
                    let dial2 = dial2
                        .map_err(EitherError::B)
                        .map_ok(EitherOutput::Second)
                        .boxed();

                    Ok(future::select_ok([dial1, dial2])
                        .map_ok(|res| res.0)
                        .boxed())
                }
            }
        }
    }
}

impl<A, B> Transport for CombineTransport<A, B>
where
    B: Transport,
    A: Transport,
    A::Output: Send + 'static,
    B::Output: Send + 'static,
    A::Error: Send + 'static,
    B::Error: Send + 'static,
    A::Dial: Send + 'static,
    B::Dial: Send + 'static,
{
    type Output = EitherOutput<A::Output, B::Output>;
    type Error = EitherError<A::Error, B::Error>;
    type ListenerUpgrade = EitherFuture<A::ListenerUpgrade, B::ListenerUpgrade>;
    type Dial = BoxFuture<
        'static,
        Result<EitherOutput<A::Output, B::Output>, EitherError<A::Error, B::Error>>,
    >;

    fn listen_on(&mut self, addr: Multiaddr) -> Result<ListenerId, TransportError<Self::Error>> {
        let id1 = self.t1.listen_on(addr.clone()).map_err(|err| match err {
            TransportError::MultiaddrNotSupported(addr) => {
                TransportError::MultiaddrNotSupported(addr)
            }
            TransportError::Other(err) => TransportError::Other(EitherError::A(err)),
        })?;
        let id2 = self.t2.listen_on(addr).map_err(|err| match err {
            TransportError::MultiaddrNotSupported(addr) => {
                TransportError::MultiaddrNotSupported(addr)
            }
            TransportError::Other(err) => TransportError::Other(EitherError::B(err)),
        })?;

        let listener_id = ListenerId::new();
        self.listener_ids.insert(listener_id, (id1, id2));

        Ok(listener_id)
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        if let Some((id1, id2)) = self.listener_ids.remove(&id) {
            self.t1.remove_listener(id1) || self.t2.remove_listener(id2)
        } else {
            false
        }
    }

    fn dial(&mut self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        self._dial(addr, false)
    }

    fn dial_as_listener(
        &mut self,
        addr: Multiaddr,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        self._dial(addr, true)
    }

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        let this = self.project();
        match this.t1.poll(cx) {
            Poll::Ready(ev) => {
                return Poll::Ready(ev.map_upgrade(EitherFuture::First).map_err(EitherError::A))
            }
            Poll::Pending => {}
        }
        match this.t2.poll(cx) {
            Poll::Ready(ev) => {
                return Poll::Ready(ev.map_upgrade(EitherFuture::Second).map_err(EitherError::B))
            }
            Poll::Pending => {}
        }
        Poll::Pending
    }

    fn address_translation(&self, server: &Multiaddr, observed: &Multiaddr) -> Option<Multiaddr> {
        self.t1
            .address_translation(server, observed)
            .or_else(|| self.t2.address_translation(server, observed))
    }
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
