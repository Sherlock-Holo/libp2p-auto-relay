use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::task::{Context, Poll};

use futures_channel::mpsc;
use futures_util::future::Either;
use futures_util::task::AtomicWaker;
use libp2p_core::{ConnectedPoint, Multiaddr, PeerId};
use libp2p_swarm::handler::{
    ConnectionEvent, DialUpgradeError, FullyNegotiatedInbound, FullyNegotiatedOutbound,
    ListenUpgradeError,
};
use libp2p_swarm::{
    ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive, SubstreamProtocol,
};
use thiserror::Error;
use tracing::instrument;

use super::event::{ConnectionHandlerInEvent, ConnectionHandlerOutEvent};
use super::upgrade::{ConnectRequest, InboundUpgrade, OutboundUpgrade};

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
    connect_request_sender: mpsc::Sender<ConnectRequest>,
}

impl IntoConnectionHandler {
    pub fn new(connect_request_sender: mpsc::Sender<ConnectRequest>) -> Self {
        Self {
            keepalive: KeepAlive::Yes,
            connect_request_sender,
        }
    }
}

impl libp2p_swarm::IntoConnectionHandler for IntoConnectionHandler {
    type Handler = ConnectionHandler;

    fn into_handler(
        self,
        _remote_peer_id: &PeerId,
        connected_point: &ConnectedPoint,
    ) -> Self::Handler {
        ConnectionHandler {
            keepalive: self.keepalive,
            remote_addr: connected_point.get_remote_address().clone(),
            connect_request_sender: self.connect_request_sender,
            pending_events: Default::default(),
            waker: Default::default(),
        }
    }

    fn inbound_protocol(
        &self,
    ) -> <Self::Handler as libp2p_swarm::ConnectionHandler>::InboundProtocol {
        InboundUpgrade::new(None, self.connect_request_sender.clone())
    }
}

#[derive(Debug)]
pub struct ConnectionHandler {
    keepalive: KeepAlive,
    remote_addr: Multiaddr,
    connect_request_sender: mpsc::Sender<ConnectRequest>,
    pending_events: PendingEvents,
    waker: AtomicWaker,
}

impl libp2p_swarm::ConnectionHandler for ConnectionHandler {
    type InEvent = ConnectionHandlerInEvent;
    type OutEvent = ConnectionHandlerOutEvent;
    type Error = Error;
    type InboundProtocol = InboundUpgrade;
    type OutboundProtocol = OutboundUpgrade;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = OutboundOpenInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(
            InboundUpgrade::new(
                Some(self.remote_addr.clone()),
                self.connect_request_sender.clone(),
            ),
            (),
        )
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keepalive
    }

    #[instrument(level = "debug")]
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<SelfConnectionHandlerEvent> {
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
            ConnectionHandlerInEvent::Connect {
                dst_addr,
                dst_peer_id,
                dialer_addr,
            } => {
                self.pending_events
                    .push_back(ConnectionHandlerEvent::OutboundSubstreamRequest {
                        protocol: SubstreamProtocol::new(
                            OutboundUpgrade::new(dialer_addr, dst_peer_id, dst_addr.clone()),
                            OutboundOpenInfo {
                                dst_addr,
                                dst_peer_id,
                            },
                        ),
                    });
            }
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
                match protocol {
                    Either::Left((dialer_connection, connection)) => {
                        self.pending_events
                            .push_back(ConnectionHandlerEvent::Custom(
                                ConnectionHandlerOutEvent::DialSuccess {
                                    dialer_connection,
                                    connection,
                                },
                            ));
                    }

                    Either::Right(listen_req) => {
                        self.pending_events
                            .push_back(ConnectionHandlerEvent::Custom(
                                ConnectionHandlerOutEvent::ListenSuccess {
                                    listen_peer_id: listen_req.listen_peer_id,
                                    listen_addr: listen_req.listen_addr,
                                },
                            ));
                    }
                }

                self.waker.wake();
            }

            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol,
                info,
            }) => {
                self.pending_events
                    .push_back(ConnectionHandlerEvent::Custom(
                        ConnectionHandlerOutEvent::ConnectSuccess {
                            connection: protocol,
                            dst_addr: info.dst_addr,
                            dst_peer_id: info.dst_peer_id,
                        },
                    ));

                self.waker.wake();
            }

            ConnectionEvent::AddressChange(_) => {}
            ConnectionEvent::DialUpgradeError(DialUpgradeError { error, info }) => {
                let err = match error {
                    ConnectionHandlerUpgrErr::Timeout => io::Error::from(ErrorKind::TimedOut),
                    _ => io::Error::new(ErrorKind::Other, error),
                };

                self.pending_events
                    .push_back(ConnectionHandlerEvent::Custom(
                        ConnectionHandlerOutEvent::ConnectFailed {
                            err,
                            dst_addr: info.dst_addr,
                            dst_peer_id: info.dst_peer_id,
                        },
                    ));

                self.waker.wake();
            }

            ConnectionEvent::ListenUpgradeError(ListenUpgradeError { error, .. }) => {
                let err = match error {
                    ConnectionHandlerUpgrErr::Timeout => io::Error::from(ErrorKind::TimedOut),
                    _ => io::Error::new(ErrorKind::Other, error),
                };

                self.pending_events
                    .push_back(ConnectionHandlerEvent::Custom(
                        ConnectionHandlerOutEvent::ListenOrDialFailed { err },
                    ));

                self.waker.wake();
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {}

#[derive(Debug)]
pub struct OutboundOpenInfo {
    dst_addr: Multiaddr,
    dst_peer_id: PeerId,
}
