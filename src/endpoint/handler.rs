use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::task::{Context, Poll};

use futures_util::future::Either;
use futures_util::task::AtomicWaker;
use libp2p_core::{ConnectedPoint, PeerId};
use libp2p_swarm::handler::{
    ConnectionEvent, DialUpgradeError, FullyNegotiatedInbound, FullyNegotiatedOutbound,
    ListenUpgradeError,
};
use libp2p_swarm::{
    ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive, SubstreamProtocol,
};
use tracing::{error, instrument};

use super::event::{
    ConnectionHandlerError, ConnectionHandlerInEvent, ConnectionHandlerOutEvent, OutboundOpenInfo,
};
use super::upgrade::{InboundUpgrade, InboundUpgradeOutput, OutboundUpgrade};

type PendingEvents = VecDeque<
    ConnectionHandlerEvent<
        <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutboundProtocol,
        <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutboundOpenInfo,
        <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutEvent,
        <ConnectionHandler as libp2p_swarm::ConnectionHandler>::Error,
    >,
>;

type SelfConnectionHandlerEvent = ConnectionHandlerEvent<
    <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutboundProtocol,
    <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutboundOpenInfo,
    <ConnectionHandler as libp2p_swarm::ConnectionHandler>::OutEvent,
    <ConnectionHandler as libp2p_swarm::ConnectionHandler>::Error,
>;

#[derive(Debug)]
pub struct IntoConnectionHandler {
    keepalive: KeepAlive,
    local_peer_id: PeerId,
}

impl IntoConnectionHandler {
    pub fn new(local_peer_id: PeerId) -> Self {
        Self {
            keepalive: KeepAlive::Yes,
            local_peer_id,
        }
    }
}

impl libp2p_swarm::IntoConnectionHandler for IntoConnectionHandler {
    type Handler = ConnectionHandler;

    fn into_handler(
        self,
        remote_peer_id: &PeerId,
        _connected_point: &ConnectedPoint,
    ) -> Self::Handler {
        ConnectionHandler {
            local_peer_id: self.local_peer_id,
            relay_peer_id: *remote_peer_id,
            keepalive: self.keepalive,
            pending_events: Default::default(),
            pending_inbound_upgrade_output: Default::default(),
            waker: Default::default(),
        }
    }

    fn inbound_protocol(
        &self,
    ) -> <Self::Handler as libp2p_swarm::ConnectionHandler>::InboundProtocol {
        InboundUpgrade::new(self.local_peer_id, None)
    }
}

#[derive(Debug)]
pub struct ConnectionHandler {
    local_peer_id: PeerId,
    relay_peer_id: PeerId,
    keepalive: KeepAlive,
    pending_events: PendingEvents,
    pending_inbound_upgrade_output: VecDeque<InboundUpgradeOutput>,
    waker: AtomicWaker,
}

impl libp2p_swarm::ConnectionHandler for ConnectionHandler {
    type InEvent = ConnectionHandlerInEvent;
    type OutEvent = ConnectionHandlerOutEvent;
    type Error = ConnectionHandlerError;
    type InboundProtocol = InboundUpgrade;
    type OutboundProtocol = OutboundUpgrade;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = OutboundOpenInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(
            InboundUpgrade::new(self.local_peer_id, Some(self.relay_peer_id)),
            (),
        )
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keepalive
    }

    #[instrument(level = "debug")]
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<SelfConnectionHandlerEvent> {
        if let Some(inbound_upgrade_output) = self.pending_inbound_upgrade_output.pop_front() {
            return Poll::Ready(ConnectionHandlerEvent::Custom(
                ConnectionHandlerOutEvent::NewConnection {
                    relay_peer_id: inbound_upgrade_output.relay_peer_id,
                    listen_addr: inbound_upgrade_output.listen_addr,
                    connection: inbound_upgrade_output.connection,
                },
            ));
        }

        if let Some(event) = self.pending_events.pop_front() {
            Poll::Ready(event)
        } else {
            self.waker.register(cx.waker());

            Poll::Pending
        }
    }

    #[instrument(level = "debug")]
    fn on_behaviour_event(&mut self, event: Self::InEvent) {
        match event {
            ConnectionHandlerInEvent::Dial {
                dst_peer_id,
                dst_addr,
                connection_sender,
            } => self
                .pending_events
                .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                    protocol: SubstreamProtocol::new(
                        OutboundUpgrade::new_dial(dst_peer_id, dst_addr),
                        OutboundOpenInfo::Dial { connection_sender },
                    ),
                }),

            ConnectionHandlerInEvent::Listen {
                listener_id,
                local_peer_id,
                local_addr,
            } => self
                .pending_events
                .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                    protocol: SubstreamProtocol::new(
                        OutboundUpgrade::new_listen(local_peer_id, local_addr.clone()),
                        OutboundOpenInfo::Listen {
                            listener_id,
                            local_peer_id,
                            listen_addr: local_addr,
                        },
                    ),
                }),
        }

        self.waker.wake();
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound {
                protocol, ..
            }) => {
                self.pending_inbound_upgrade_output.push_back(protocol);
            }

            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol,
                info,
            }) => match (protocol, info) {
                (Either::Left(connection), OutboundOpenInfo::Dial { connection_sender }) => {
                    self.pending_events
                        .push_back(ConnectionHandlerEvent::Custom(
                            ConnectionHandlerOutEvent::DialSuccess {
                                relay_peer_id: self.relay_peer_id,
                                connection,
                                sender: connection_sender,
                            },
                        ));
                }

                (
                    Either::Right(_),
                    OutboundOpenInfo::Listen {
                        listener_id,
                        local_peer_id,
                        listen_addr,
                    },
                ) => {
                    self.pending_events
                        .push_back(ConnectionHandlerEvent::Custom(
                            ConnectionHandlerOutEvent::ListenSuccess {
                                relay_peer_id: self.relay_peer_id,
                                listener_id,
                                local_peer_id,
                                listen_addr,
                            },
                        ));
                }

                _ => unreachable!(),
            },

            ConnectionEvent::AddressChange(_) => return,

            ConnectionEvent::DialUpgradeError(DialUpgradeError { info, error: err }) => {
                let err = match err {
                    ConnectionHandlerUpgrErr::Timeout => io::Error::from(ErrorKind::TimedOut),
                    _ => io::Error::new(ErrorKind::Other, err),
                };

                match info {
                    OutboundOpenInfo::Dial { connection_sender } => {
                        self.pending_events
                            .push_back(ConnectionHandlerEvent::Custom(
                                ConnectionHandlerOutEvent::DialFailed {
                                    err,
                                    relay_peer_id: self.relay_peer_id,
                                    sender: connection_sender,
                                },
                            ))
                    }

                    OutboundOpenInfo::Listen {
                        listener_id,
                        local_peer_id,
                        listen_addr,
                    } => {
                        self.pending_events
                            .push_back(ConnectionHandlerEvent::Custom(
                                ConnectionHandlerOutEvent::ListenFailed {
                                    err,
                                    relay_peer_id: self.relay_peer_id,
                                    listener_id,
                                    local_peer_id,
                                    listen_addr,
                                },
                            ));
                    }
                }
            }

            ConnectionEvent::ListenUpgradeError(ListenUpgradeError { error: err, .. }) => {
                error!(%err, "accept connection failed");

                return;
            }
        }

        self.waker.wake();
    }
}
