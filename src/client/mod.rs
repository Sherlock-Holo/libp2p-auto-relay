use std::collections::{HashMap, VecDeque};
use std::io;
use std::task::{ready, Context, Poll};

use futures_channel::mpsc::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use libp2p_core::connection::ConnectionId;
use libp2p_core::transport::ListenerId;
use libp2p_core::{Multiaddr, PeerId};
use libp2p_swarm::behaviour::{ConnectionClosed, ConnectionEstablished, FromSwarm};
use libp2p_swarm::{
    ConnectionHandler, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler, PollParameters,
};
use tracing::{debug, error, instrument};

use self::connection::Connection;
pub use self::event::Event;
use self::event::{BehaviourToTransportEvent, TransportToBehaviourEvent};
use self::event::{ConnectionHandlerInEvent, ConnectionHandlerOutEvent};
use self::handler::IntoConnectionHandler;
pub use self::transport::{Error, Transport};

mod connection;
mod event;
mod handler;
mod transport;
mod upgrade;

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
    pending_to_transport: VecDeque<BehaviourToTransportEvent>,
    to_transport: Sender<BehaviourToTransportEvent>,
}

impl Behaviour {
    pub(crate) fn new(
        peer_id: PeerId,
        from_transport: Receiver<TransportToBehaviourEvent>,
        to_transport: Sender<BehaviourToTransportEvent>,
    ) -> Self {
        Self {
            from_transport,
            peer_id,
            listeners: Default::default(),
            pending_new_connections: Default::default(),
            pending_actions: Default::default(),
            pending_to_transport: Default::default(),
            to_transport,
        }
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = IntoConnectionHandler;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        IntoConnectionHandler::default()
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(ConnectionEstablished { .. }) => {}

            FromSwarm::ConnectionClosed(ConnectionClosed {
                peer_id,
                connection_id,
                endpoint,
                ..
            }) => {
                self.pending_to_transport
                    .push_back(BehaviourToTransportEvent::PeerClosed {
                        peer_id,
                        peer_addr: endpoint.get_remote_address().clone(),
                        connection_id,
                    });
            }
            FromSwarm::AddressChange(_) => {}
            FromSwarm::DialFailure(_) => {}
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
                listen_addr,
                connection,
            } => {
                self.pending_new_connections
                    .push_back(PendingNewConnection {
                        listen_addr,
                        connection,
                    });
            }
            ConnectionHandlerOutEvent::Error(err) => {
                self.pending_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(Event::OtherError(
                        err,
                    )));
            }

            ConnectionHandlerOutEvent::DialSuccess { connection, sender } => {
                if let Err(_err) = sender.send(Ok(connection)) {
                    debug!("connection dial is canceled");
                } else {
                    debug!("connection dial done, send back to transport");
                }
            }

            ConnectionHandlerOutEvent::ListenSuccess {
                listener_id,
                local_peer_id,
                listen_addr,
            } => {
                debug!(?listener_id, %local_peer_id, %listen_addr, "listen done");

                self.pending_to_transport
                    .push_back(BehaviourToTransportEvent::ListenSuccess {
                        listener_id,
                        local_addr: listen_addr,
                    });
            }

            ConnectionHandlerOutEvent::DialFailed { err, sender } => {
                error!(%err, "dial failed");

                let _ = sender.send(Err(err));
            }

            ConnectionHandlerOutEvent::ListenFailed {
                err,
                listener_id,
                listen_addr,
                ..
            } => {
                error!(%err, "listen failed");

                self.pending_to_transport
                    .push_back(BehaviourToTransportEvent::ListenFailed {
                        err,
                        listener_id,
                        local_addr: listen_addr,
                    })
            }
        }
    }

    #[instrument(level = "debug", skip(_params))]
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        if let Some(transport_to_behaviour_event) = ready!(self.from_transport.poll_next_unpin(cx))
        {
            return match transport_to_behaviour_event {
                TransportToBehaviourEvent::Dial {
                    dst_addr,
                    relay_addr: _relay_addr,
                    connection_sender,
                } => Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                    peer_id: self.peer_id,
                    handler: NotifyHandler::Any,
                    event: ConnectionHandlerInEvent::Dial {
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

            self.pending_to_transport
                .push_back(BehaviourToTransportEvent::ListenSuccess {
                    listener_id: listener_sender_with_id.listener_id,
                    local_addr: new_connection.listen_addr,
                });
        }

        while let Some(behaviour_to_transport_event) = self.pending_to_transport.pop_front() {
            match self.to_transport.poll_ready_unpin(cx) {
                Poll::Pending => {
                    // wait for next times
                    self.pending_to_transport
                        .push_front(behaviour_to_transport_event);

                    break;
                }
                Poll::Ready(Err(err)) => {
                    error!(%err, "transport is dropped");

                    return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                        Event::UnexpectedTransportDropped,
                    ));
                }

                Poll::Ready(Ok(_)) => {}
            }

            if let Err(err) = self
                .to_transport
                .start_send_unpin(behaviour_to_transport_event)
            {
                error!(%err, "transport is dropped");

                return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                    Event::UnexpectedTransportDropped,
                ));
            }
        }

        if let Poll::Ready(Err(err)) = self.to_transport.poll_flush_unpin(cx) {
            error!(%err, "transport is dropped");

            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                Event::UnexpectedTransportDropped,
            ));
        }

        if let Some(action) = self.pending_actions.pop_front() {
            Poll::Ready(action)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
struct PendingNewConnection {
    listen_addr: Multiaddr,
    connection: Connection,
}

#[derive(Debug)]
struct ListenerSenderWithId {
    listener_id: ListenerId,
    sender: Sender<io::Result<Connection>>,
}
