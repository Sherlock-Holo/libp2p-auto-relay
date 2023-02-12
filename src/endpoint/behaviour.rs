use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::io::ErrorKind;
use std::task::{Context, Poll};

use futures_channel::mpsc::{Receiver, Sender};
use futures_util::task::AtomicWaker;
use futures_util::{SinkExt, StreamExt};
use libp2p_core::connection::ConnectionId;
use libp2p_core::transport::ListenerId;
use libp2p_core::{Multiaddr, PeerId};
use libp2p_swarm::behaviour::{ConnectionClosed, ConnectionEstablished, DialFailure, FromSwarm};
use libp2p_swarm::dial_opts::DialOpts;
use libp2p_swarm::{
    ConnectionHandler, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler, PollParameters,
};
use tracing::{debug, error, instrument};

use super::event::{BehaviourToTransportEvent, TransportToBehaviourEvent};
use super::event::{ConnectionHandlerInEvent, ConnectionHandlerOutEvent};
use super::handler::IntoConnectionHandler;
use super::Event;
use crate::connection::Connection;

#[derive(Debug)]
pub struct Behaviour {
    from_transport: Receiver<TransportToBehaviourEvent>,
    peer_id: PeerId,
    listeners: HashMap<Multiaddr, ListenerSenderWithId>,
    pending_new_connections: VecDeque<PendingNewConnection>,
    pending_actions: VecDeque<
        NetworkBehaviourAction<
            <Self as NetworkBehaviour>::OutEvent,
            <Self as NetworkBehaviour>::ConnectionHandler,
        >,
    >,
    pending_to_transport: HashMap<PeerId, VecDeque<BehaviourToTransportEvent>>,
    to_transports: HashMap<PeerId, Sender<BehaviourToTransportEvent>>,
    established_relay_connection: HashMap<PeerId, HashSet<ConnectionId>>,
    connecting_relay: HashSet<PeerId>,
    wait_connection_transport_to_behaviour_events: HashMap<PeerId, Vec<TransportToBehaviourEvent>>,
    pending_transport_to_behaviour_events: VecDeque<TransportToBehaviourEvent>,
    waker: AtomicWaker,
}

impl Behaviour {
    pub(crate) fn new(
        peer_id: PeerId,
        from_transport: Receiver<TransportToBehaviourEvent>,
        to_transports: HashMap<PeerId, Sender<BehaviourToTransportEvent>>,
    ) -> Self {
        let pending_to_transport = to_transports
            .keys()
            .map(|peer_id| (*peer_id, VecDeque::new()))
            .collect();

        Self {
            from_transport,
            peer_id,
            listeners: Default::default(),
            pending_new_connections: Default::default(),
            pending_actions: Default::default(),
            pending_to_transport,
            to_transports,
            established_relay_connection: Default::default(),
            connecting_relay: Default::default(),
            wait_connection_transport_to_behaviour_events: Default::default(),
            pending_transport_to_behaviour_events: Default::default(),
            waker: Default::default(),
        }
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = IntoConnectionHandler;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        IntoConnectionHandler::new(self.peer_id)
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(ConnectionEstablished {
                peer_id,
                connection_id,
                endpoint,
                ..
            }) => {
                if self.connecting_relay.remove(&peer_id) {
                    debug!(
                        %peer_id, ?connection_id, peer_addr = %endpoint.get_remote_address(),
                        "connect relay done"
                    );

                    self.established_relay_connection
                        .entry(peer_id)
                        .or_default()
                        .insert(connection_id);

                    if let Some(events) = self
                        .wait_connection_transport_to_behaviour_events
                        .remove(&peer_id)
                    {
                        self.pending_transport_to_behaviour_events.extend(events);

                        self.waker.wake();
                    }
                }
            }

            FromSwarm::ConnectionClosed(ConnectionClosed {
                peer_id,
                connection_id,
                endpoint,
                ..
            }) => {
                if let Some(pending_to_transport) = self.pending_to_transport.get_mut(&peer_id) {
                    pending_to_transport.push_back(BehaviourToTransportEvent::PeerClosed {
                        peer_id,
                        peer_addr: endpoint.get_remote_address().clone(),
                        connection_id,
                    });
                } else {
                    error!(
                        relay_peer_id = %peer_id,
                        "relay peer pending to transport event queue not found"
                    );
                }

                if let Some(connections) = self.established_relay_connection.get_mut(&peer_id) {
                    connections.remove(&connection_id);
                    if connections.is_empty() {
                        self.established_relay_connection.remove(&peer_id);
                    }
                }

                self.waker.wake();
            }
            FromSwarm::AddressChange(_) => {}
            FromSwarm::DialFailure(DialFailure { peer_id, error, .. }) => {
                error!(?peer_id, %error, "dial peer failed");

                if let Some(peer_id) = peer_id {
                    if let Some(events) = self
                        .wait_connection_transport_to_behaviour_events
                        .remove(&peer_id)
                    {
                        let behaviour_to_transport_events =
                            events.into_iter().map(|event| match event {
                                TransportToBehaviourEvent::Dial {
                                    dst_peer_id,
                                    dst_addr,
                                    connection_sender,
                                    ..
                                } => {
                                    let _ = connection_sender.send(Err(io::Error::new(
                                        ErrorKind::Other,
                                        format!("dial {dst_peer_id} {dst_addr} failed: {error}"),
                                    )));

                                    BehaviourToTransportEvent::DialFailed {
                                        err: io::Error::new(
                                            ErrorKind::Other,
                                            format!(
                                                "dial {dst_peer_id} {dst_addr} failed: {error}"
                                            ),
                                        ),
                                        dst_peer_id,
                                        dst_addr,
                                    }
                                }
                                TransportToBehaviourEvent::Listen {
                                    listener_id,
                                    local_peer_id,
                                    local_addr,
                                    ..
                                } => BehaviourToTransportEvent::ListenFailed {
                                    err: io::Error::new(
                                        ErrorKind::Other,
                                        format!(
                                            "listen {local_peer_id} {local_addr} failed: {error}"
                                        ),
                                    ),
                                    listener_id,
                                    local_addr,
                                },
                            });

                        if let Some(pending_to_transport) =
                            self.pending_to_transport.get_mut(&peer_id)
                        {
                            pending_to_transport.extend(behaviour_to_transport_events);
                        } else {
                            error!(
                                relay_peer_id = %peer_id,
                                "relay peer pending to transport event queue not found"
                            );
                        }

                        self.waker.wake();
                    }
                }
            }
            FromSwarm::ListenFailure(_) => {}
            FromSwarm::NewListener(_) => {}
            FromSwarm::NewListenAddr(_) => {}
            FromSwarm::ExpiredListenAddr(_) => {}
            FromSwarm::ListenerError(_) => {}
            FromSwarm::ListenerClosed(_) => {}
            FromSwarm::NewExternalAddr(_) => {}
            FromSwarm::ExpiredExternalAddr(_) => {}
        }
    }

    #[instrument(level = "debug")]
    fn on_connection_handler_event(
        &mut self,
        _peer_id: PeerId,
        _connection_id: ConnectionId,
        event: <<Self::ConnectionHandler as libp2p_swarm::IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        match event {
            ConnectionHandlerOutEvent::NewConnection {
                relay_peer_id,
                listen_addr,
                connection,
            } => {
                self.pending_new_connections
                    .push_back(PendingNewConnection {
                        listen_addr,
                        relay_peer_id,
                        connection,
                    });
            }
            ConnectionHandlerOutEvent::Error(err) => {
                self.pending_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(Event::OtherError(
                        err,
                    )));
            }

            ConnectionHandlerOutEvent::DialSuccess {
                connection, sender, ..
            } => {
                if let Err(_err) = sender.send(Ok(connection)) {
                    debug!("connection dial is canceled");
                } else {
                    debug!("connection dial done, send back to transport");
                }

                return;
            }

            ConnectionHandlerOutEvent::ListenSuccess {
                relay_peer_id,
                listener_id,
                local_peer_id,
                listen_addr,
            } => {
                debug!(?listener_id, %local_peer_id, %listen_addr, "listen done");

                if let Some(pending_to_transport) =
                    self.pending_to_transport.get_mut(&relay_peer_id)
                {
                    pending_to_transport.push_back(BehaviourToTransportEvent::ListenSuccess {
                        listener_id,
                        listen_addr,
                    });
                } else {
                    error!(%relay_peer_id, "relay peer pending to transport event queue not found");
                }
            }

            ConnectionHandlerOutEvent::DialFailed { err, sender, .. } => {
                error!(%err, "dial failed");

                let _ = sender.send(Err(err));

                return;
            }

            ConnectionHandlerOutEvent::ListenFailed {
                err,
                relay_peer_id,
                listener_id,
                listen_addr,
                ..
            } => {
                error!(%err, "listen failed");

                if let Some(pending_to_transport) =
                    self.pending_to_transport.get_mut(&relay_peer_id)
                {
                    pending_to_transport.push_back(BehaviourToTransportEvent::ListenFailed {
                        err,
                        listener_id,
                        local_addr: listen_addr,
                    });
                } else {
                    error!(%relay_peer_id, "relay peer pending to transport event queue not found");
                }
            }
        }

        self.waker.wake();
    }

    #[instrument(level = "debug", skip(_params))]
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        let poll_available_transport_to_behaviour_event =
            if let Poll::Ready(Some(event)) = self.from_transport.poll_next_unpin(cx) {
                let relay_peer_id = event.relay_peer_id();
                let relay_addr = event.relay_addr();
                // if relay is not established, need dial first
                if !self
                    .established_relay_connection
                    .contains_key(&relay_peer_id)
                {
                    self.wait_connection_transport_to_behaviour_events
                        .entry(relay_peer_id)
                        .or_default()
                        .push(event);

                    if !self.connecting_relay.contains(&relay_peer_id) {
                        self.connecting_relay.insert(relay_peer_id);
                        let handler = self.new_handler();

                        return Poll::Ready(NetworkBehaviourAction::Dial {
                            opts: DialOpts::peer_id(relay_peer_id)
                                .addresses(vec![relay_addr])
                                .build(),
                            handler,
                        });
                    }

                    None
                } else {
                    Some(event)
                }
            } else {
                self.pending_transport_to_behaviour_events.pop_front()
            };

        if let Some(event) = poll_available_transport_to_behaviour_event {
            debug!(
                ?event,
                "get poll available transport to behaviour event done"
            );

            return match event {
                TransportToBehaviourEvent::Dial {
                    dst_peer_id,
                    dst_addr,
                    relay_addr: _relay_addr,
                    relay_peer_id,
                    connection_sender,
                } => Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                    peer_id: relay_peer_id,
                    handler: NotifyHandler::Any,
                    event: ConnectionHandlerInEvent::Dial {
                        dst_peer_id,
                        dst_addr,
                        connection_sender,
                    },
                }),
                TransportToBehaviourEvent::Listen {
                    listener_id,
                    local_peer_id,
                    local_addr,
                    relay_peer_id,
                    relay_addr: _relay_addr,
                    connection_sender,
                } => {
                    self.listeners.insert(
                        local_addr.clone(),
                        ListenerSenderWithId {
                            listener_id,
                            sender: connection_sender,
                        },
                    );

                    Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id: relay_peer_id,
                        handler: NotifyHandler::Any,
                        event: ConnectionHandlerInEvent::Listen {
                            listener_id,
                            local_peer_id,
                            local_addr,
                        },
                    })
                }
            };
        }

        while let Some(new_connection) = self.pending_new_connections.pop_front() {
            let listener_sender_with_id = match self.listeners.get_mut(&new_connection.listen_addr)
            {
                None => {
                    error!(listen_addr = %new_connection.listen_addr, "unexpected new connection");

                    return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                        Event::UnexpectedConnection {
                            listen_addr: new_connection.listen_addr,
                        },
                    ));
                }

                Some(sender) => sender,
            };

            match listener_sender_with_id.sender.poll_ready_unpin(cx) {
                Poll::Pending => {
                    // wait for next times poll
                    self.pending_new_connections.push_front(new_connection);

                    break;
                }

                Poll::Ready(Err(err)) => {
                    error!(%err, listen_addr = %new_connection.listen_addr, "listener sender is dropped");

                    return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                        Event::UnexpectedListenerClosed {
                            listen_addr: new_connection.listen_addr,
                            err: Box::new(err),
                        },
                    ));
                }

                Poll::Ready(Ok(_)) => {}
            }

            if let Err(err) = listener_sender_with_id
                .sender
                .start_send_unpin(Ok(new_connection.connection))
            {
                error!(%err, listen_addr = %new_connection.listen_addr, "listener sender is dropped");

                return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                    Event::UnexpectedListenerClosed {
                        listen_addr: new_connection.listen_addr,
                        err: Box::new(err),
                    },
                ));
            }

            if let Poll::Ready(Err(err)) = listener_sender_with_id.sender.poll_flush_unpin(cx) {
                error!(%err, listen_addr = %new_connection.listen_addr, "listener sender is dropped");

                return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                    Event::UnexpectedListenerClosed {
                        listen_addr: new_connection.listen_addr,
                        err: Box::new(err),
                    },
                ));
            }

            let pending_to_transport = self
                .pending_to_transport
                .get_mut(&new_connection.relay_peer_id)
                .unwrap_or_else(|| {
                    panic!(
                        "relay peer {} pending to transport event queue not found",
                        new_connection.relay_peer_id
                    )
                });

            pending_to_transport.push_back(BehaviourToTransportEvent::ListenSuccess {
                listener_id: listener_sender_with_id.listener_id,
                listen_addr: new_connection.listen_addr,
            });
        }

        for (relay_peer_id, pending_to_transport) in self.pending_to_transport.iter_mut() {
            let to_transport = self
                .to_transports
                .get_mut(relay_peer_id)
                .unwrap_or_else(|| {
                    panic!("relay peer {relay_peer_id} to transport sender not found")
                });

            while let Some(event) = pending_to_transport.pop_front() {
                match to_transport.poll_ready_unpin(cx) {
                    Poll::Pending => {
                        // wait for next times
                        pending_to_transport.push_front(event);

                        break;
                    }

                    Poll::Ready(Err(err)) => {
                        error!(%err, "transport is dropped");

                        let relay_peer_id = *relay_peer_id;

                        // remove it to avoid panic
                        self.pending_to_transport.remove(&relay_peer_id);
                        self.to_transports.remove(&relay_peer_id);

                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                            Event::UnexpectedTransportDropped,
                        ));
                    }

                    Poll::Ready(Ok(_)) => {}
                }

                if let Err(err) = to_transport.start_send_unpin(event) {
                    error!(%err, "transport is dropped");

                    return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                        Event::UnexpectedTransportDropped,
                    ));
                }
            }
        }

        if let Some(action) = self.pending_actions.pop_front() {
            Poll::Ready(action)
        } else {
            self.waker.register(cx.waker());

            Poll::Pending
        }
    }
}

#[derive(Debug)]
struct PendingNewConnection {
    listen_addr: Multiaddr,
    relay_peer_id: PeerId,
    connection: Connection,
}

#[derive(Debug)]
struct ListenerSenderWithId {
    listener_id: ListenerId,
    sender: Sender<io::Result<Connection>>,
}
