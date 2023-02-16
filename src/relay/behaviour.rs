use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::task::{Context, Poll};

use futures_channel::{mpsc, oneshot};
use futures_util::stream::FuturesUnordered;
use futures_util::task::AtomicWaker;
use futures_util::{io, StreamExt, TryStreamExt};
use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite};
use libp2p_core::connection::ConnectionId;
use libp2p_core::PeerId;
use libp2p_swarm::behaviour::FromSwarm;
use libp2p_swarm::{
    ConnectionHandler, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler, PollParameters,
};
use tracing::{debug, debug_span, error, instrument, warn, Instrument};

use super::event::ConnectionHandlerOutEvent;
use super::handler::IntoConnectionHandler;
use super::upgrade::ConnectRequest;
use super::Event;
use crate::connection::Connection;
use crate::relay::event::ConnectionHandlerInEvent;

type CopyFuture = impl Future<Output = io::Result<u64>>;

/// the relay server behaviour
///
/// this behaviour will communicate with endpoints, accept dial and listen request and respond them
/// then forward the data
#[derive(Debug)]
pub struct Behaviour {
    listening_clients: HashSet<PeerId>,
    connect_request_sender: mpsc::Sender<ConnectRequest>,
    connect_request_receiver: mpsc::Receiver<ConnectRequest>,
    connecting_requests: HashMap<PeerId, oneshot::Sender<io::Result<Connection>>>,
    io_copy_futs: FuturesUnordered<CopyFuture>,
    waker: AtomicWaker,
    pending_actions: VecDeque<
        NetworkBehaviourAction<
            <Self as NetworkBehaviour>::OutEvent,
            <Self as NetworkBehaviour>::ConnectionHandler,
        >,
    >,
}

impl Behaviour {
    /// create a relay server behaviour
    pub fn new() -> Self {
        let (connect_request_sender, connect_request_receiver) = mpsc::channel(10);

        Self {
            listening_clients: Default::default(),
            connect_request_sender,
            connect_request_receiver,
            connecting_requests: Default::default(),
            io_copy_futs: Default::default(),
            waker: Default::default(),
            pending_actions: Default::default(),
        }
    }
}

impl Default for Behaviour {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = IntoConnectionHandler;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        IntoConnectionHandler::new(self.connect_request_sender.clone())
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        // TODO should I handle it?
        match event {
            FromSwarm::ConnectionEstablished(_) => {}
            FromSwarm::ConnectionClosed(_) => {}
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
            ConnectionHandlerOutEvent::DialSuccess {
                dialer_connection,
                connection,
            } => {
                let dialer_addr = dialer_connection.remote_addr();
                let dst_addr = connection.remote_addr();

                debug!(%dialer_addr, %dst_addr, "dial success");

                let event = Event::StartCopy {
                    dialer_addr: dialer_addr.clone(),
                    dst_addr: dst_addr.clone(),
                };
                let (dial_conn_read, dial_conn_write) = dialer_connection.split();
                let (conn_read, conn_write) = connection.split();

                self.io_copy_futs.push(copy(dial_conn_read, conn_write));
                self.io_copy_futs.push(copy(conn_read, dial_conn_write));
                self.pending_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(event));
            }

            ConnectionHandlerOutEvent::ConnectSuccess {
                connection,
                dst_addr,
                dst_peer_id,
            } => {
                match self.connecting_requests.remove(&dst_peer_id) {
                    None => {
                        warn!(%dst_addr, "dial request may be dropped");
                    }

                    Some(sender) => {
                        if sender.send(Ok(connection)).is_err() {
                            warn!(%dst_addr, "dial request may be dropped");
                        } else {
                            debug!("send dial request connection back done");
                        }
                    }
                }

                return;
            }

            ConnectionHandlerOutEvent::ConnectFailed {
                err,
                dst_addr,
                dst_peer_id,
            } => {
                error!(%err, %dst_addr, "connect failed");

                if let Some(sender) = self.connecting_requests.remove(&dst_peer_id) {
                    let _ = sender.send(Err(err));
                }

                return;
            }

            ConnectionHandlerOutEvent::ListenSuccess {
                listen_peer_id,
                listen_addr,
            } => {
                debug!(%listen_peer_id, %listen_addr, "listen done");

                self.listening_clients.insert(listen_peer_id);
                self.pending_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(Event::Listen {
                        listen_peer_id,
                        listen_addr,
                    }));
            }

            ConnectionHandlerOutEvent::ListenOrDialFailed { err } => {
                self.pending_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::ListenOrDialFailed { err },
                    ));
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
        if let Poll::Ready(Some(Err(err))) = self.io_copy_futs.try_poll_next_unpin(cx) {
            error!(%err, "a relay connection copy error happened");
        }

        if let Poll::Ready(Some(ConnectRequest {
            dst_peer_id,
            dst_addr,
            dialer_addr,
            connection_sender,
        })) = self.connect_request_receiver.poll_next_unpin(cx)
        {
            if !self.listening_clients.contains(&dst_peer_id) {
                error!(%dialer_addr, %dst_peer_id, %dst_addr, "dialer want to dial a not listen peer");

                let _ = connection_sender.send(Err(io::Error::new(
                    ErrorKind::ConnectionRefused,
                    format!("dst peer id {dst_peer_id} is not listened"),
                )));

                return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                    Event::ConnectToNotListenPeer {
                        dialer_addr,
                        dst_peer_id,
                    },
                ));
            }

            self.connecting_requests
                .insert(dst_peer_id, connection_sender);

            return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                peer_id: dst_peer_id,
                handler: NotifyHandler::Any,
                event: ConnectionHandlerInEvent::Connect {
                    dst_addr,
                    dst_peer_id,
                    dialer_addr,
                },
            });
        }

        if let Some(action) = self.pending_actions.pop_front() {
            return Poll::Ready(action);
        }

        self.waker.register(cx.waker());

        Poll::Pending
    }
}

#[inline]
async fn copy<R: AsyncRead, W: AsyncWrite + Unpin>(reader: R, mut writer: W) -> io::Result<u64> {
    io::copy(reader, &mut writer)
        .instrument(debug_span!("copy"))
        .await
}
