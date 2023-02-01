use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::task::{Context, Poll};

use futures_util::future::Either;
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
}

impl Default for IntoConnectionHandler {
    fn default() -> Self {
        Self {
            keepalive: KeepAlive::Yes,
        }
    }
}

impl libp2p_swarm::IntoConnectionHandler for IntoConnectionHandler {
    type Handler = ConnectionHandler;

    fn into_handler(
        self,
        _remote_peer_id: &PeerId,
        _connected_point: &ConnectedPoint,
    ) -> Self::Handler {
        ConnectionHandler {
            keepalive: self.keepalive,
            pending_events: Default::default(),
            pending_inbound_upgrade_output: Default::default(),
        }
    }

    fn inbound_protocol(
        &self,
    ) -> <Self::Handler as libp2p_swarm::ConnectionHandler>::InboundProtocol {
        InboundUpgrade {}
    }
}

#[derive(Debug)]
pub struct ConnectionHandler {
    keepalive: KeepAlive,
    pending_events: PendingEvents,
    pending_inbound_upgrade_output: VecDeque<InboundUpgradeOutput>,
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
        SubstreamProtocol::new(InboundUpgrade {}, ())
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keepalive
    }

    #[instrument(level = "debug")]
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<SelfConnectionHandlerEvent> {
        if let Some(inbound_upgrade_output) = self.pending_inbound_upgrade_output.pop_front() {
            return Poll::Ready(ConnectionHandlerEvent::Custom(
                ConnectionHandlerOutEvent::NewConnection {
                    listen_addr: inbound_upgrade_output.listen_addr,
                    connection: inbound_upgrade_output.connection,
                },
            ));
        }

        if let Some(event) = self.pending_events.pop_front() {
            Poll::Ready(event)
        } else {
            Poll::Pending
        }
    }

    #[instrument(level = "debug")]
    fn on_behaviour_event(&mut self, event: Self::InEvent) {
        match event {
            ConnectionHandlerInEvent::Dial {
                dst_addr,
                connection_sender,
            } => self
                .pending_events
                .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                    protocol: SubstreamProtocol::new(
                        OutboundUpgrade::new_dial(dst_addr),
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
                                listener_id,
                                local_peer_id,
                                listen_addr,
                            },
                        ));
                }

                _ => unreachable!(),
            },

            ConnectionEvent::AddressChange(_) => {}

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
            }
        }
    }
}
