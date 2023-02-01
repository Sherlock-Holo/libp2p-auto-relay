use std::collections::VecDeque;
use std::future::{ready, Future, Ready};
use std::io;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use futures_channel::mpsc::{Receiver, Sender};
use futures_channel::{mpsc, oneshot};
use futures_util::stream::SelectAll;
use futures_util::{SinkExt, Stream, StreamExt};
use libp2p_core::transport::{ListenerId, TransportError, TransportEvent};
use libp2p_core::{Multiaddr, PeerId};
use thiserror::Error;
use tracing::{debug_span, error, instrument, Instrument};

use super::event::{BehaviourToTransportEvent, TransportToBehaviourEvent};
use super::Behaviour;
use crate::connection::Connection;

#[derive(Debug)]
pub struct Transport {
    local_peer_id: PeerId,
    relay_addr: Multiaddr,
    relay_peer_id: PeerId,
    pending_to_behaviour: VecDeque<TransportToBehaviourEvent>,
    to_behaviour: Sender<TransportToBehaviourEvent>,
    listeners: SelectAll<Listener>,
    from_behaviour: Receiver<BehaviourToTransportEvent>,
}

impl Transport {
    pub fn new(
        local_peer_id: PeerId,
        relay_addr: Multiaddr,
        relay_peer_id: PeerId,
    ) -> (Self, Behaviour) {
        let (t_to_b_sender, t_to_b_receiver) = mpsc::channel(10);
        let (b_to_t_sender, b_to_t_receiver) = mpsc::channel(10);

        let client = Behaviour::new(local_peer_id, t_to_b_receiver, b_to_t_sender);

        (
            Self {
                local_peer_id,
                relay_addr,
                relay_peer_id,
                pending_to_behaviour: Default::default(),
                to_behaviour: t_to_b_sender,
                listeners: Default::default(),
                from_behaviour: b_to_t_receiver,
            },
            client,
        )
    }
}

impl libp2p_core::Transport for Transport {
    type Output = Connection;
    type Error = Error;
    type ListenerUpgrade = Ready<Result<Self::Output, Self::Error>>;
    type Dial = impl Future<Output = Result<Self::Output, Self::Error>>;

    #[instrument(level = "debug", err)]
    fn listen_on(&mut self, addr: Multiaddr) -> Result<ListenerId, TransportError<Self::Error>> {
        let (conn_sender, conn_receiver) = mpsc::channel(10);
        let listener_id = ListenerId::new();
        let event = TransportToBehaviourEvent::Listen {
            listener_id,
            local_peer_id: self.local_peer_id,
            local_addr: addr.clone(),
            relay_peer_id: self.relay_peer_id,
            relay_addr: self.relay_addr.clone(),
            connection_sender: conn_sender,
        };

        self.pending_to_behaviour.push_back(event);

        let listener = Listener {
            local_addr: addr,
            listener_id,
            receiver: Some(conn_receiver),
        };

        self.listeners.push(listener);

        Ok(listener_id)
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        if let Some(listener) = self
            .listeners
            .iter_mut()
            .find(|listener| listener.listener_id == id)
        {
            listener.receiver.take();

            true
        } else {
            false
        }
    }

    #[instrument(level = "debug", err)]
    fn dial(&mut self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        let (conn_sender, conn_receiver) = oneshot::channel();

        let event = TransportToBehaviourEvent::Dial {
            dst_addr: addr,
            relay_addr: self.relay_addr.clone(),
            connection_sender: conn_sender,
        };

        self.pending_to_behaviour.push_back(event);

        Ok(async move {
            let connection = conn_receiver
                .await
                .map_err(|err| {
                    error!(%err, "connection sender is dropped");

                    Error::DialFailed(io::Error::new(
                        ErrorKind::Other,
                        "connection sender is dropped",
                    ))
                })?
                .map_err(Error::DialFailed)?;

            Ok(connection)
        }
        .instrument(debug_span!("dial")))
    }

    fn dial_as_listener(
        &mut self,
        _addr: Multiaddr,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        Err::<Self::Dial, _>(Error::UnsupportedDialAsListener.into())
    }

    #[instrument(level = "debug")]
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        loop {
            match self.from_behaviour.poll_next_unpin(cx) {
                Poll::Ready(Some(behaviour_to_transport_event)) => {
                    match behaviour_to_transport_event {
                        BehaviourToTransportEvent::ListenSuccess {
                            listener_id,
                            local_addr,
                        } => {
                            return Poll::Ready(TransportEvent::NewAddress {
                                listener_id,
                                listen_addr: local_addr,
                            })
                        }

                        BehaviourToTransportEvent::ListenFailed {
                            err, listener_id, ..
                        } => {
                            self.remove_listener(listener_id);

                            return Poll::Ready(TransportEvent::ListenerError {
                                listener_id,
                                error: Error::ListenFailed(err),
                            });
                        }

                        BehaviourToTransportEvent::PeerClosed { .. } => {}
                    }
                }

                Poll::Ready(None) => panic!("behaviour is dropped but transport is still polled"),
                Poll::Pending => break,
            }
        }

        while let Some(transport_to_behaviour_event) = self.pending_to_behaviour.pop_front() {
            match self.to_behaviour.poll_ready_unpin(cx) {
                Poll::Pending => {
                    // push event into queue front again
                    self.pending_to_behaviour
                        .push_front(transport_to_behaviour_event);

                    break;
                }

                Poll::Ready(Err(_)) => panic!("behaviour is dropped but transport is still polled"),
                Poll::Ready(Ok(_)) => {}
            }

            if let Err(_err) = self
                .to_behaviour
                .start_send_unpin(transport_to_behaviour_event)
            {
                panic!("behaviour is dropped but transport is still polled");
            }
        }

        if let Poll::Ready(Err(_)) = self.to_behaviour.poll_flush_unpin(cx) {
            panic!("behaviour is dropped but transport is still polled")
        }

        if let Some(event) = ready!(self.listeners.poll_next_unpin(cx)) {
            Poll::Ready(event)
        } else {
            Poll::Pending
        }
    }

    fn address_translation(&self, _listen: &Multiaddr, _observed: &Multiaddr) -> Option<Multiaddr> {
        None
    }
}

#[derive(Debug)]
struct Listener {
    local_addr: Multiaddr,
    listener_id: ListenerId,
    receiver: Option<Receiver<io::Result<Connection>>>,
}

impl Stream for Listener {
    type Item = TransportEvent<Ready<Result<Connection, Error>>, Error>;

    #[instrument(level = "debug")]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.as_mut() {
            None => Poll::Ready(None),
            Some(receiver) => {
                Poll::Ready(ready!(receiver.poll_next_unpin(cx)).map(|conn| match conn {
                    Err(err) => {
                        error!(%err, "listener receive connection failed");

                        self.receiver.take();

                        TransportEvent::ListenerError {
                            listener_id: self.listener_id,
                            error: Error::DialFailed(err),
                        }
                    }

                    Ok(conn) => {
                        let send_back_addr = conn.remote_addr().clone();

                        TransportEvent::Incoming {
                            listener_id: self.listener_id,
                            upgrade: ready(Ok(conn)),
                            local_addr: self.local_addr.clone(),
                            send_back_addr,
                        }
                    }
                }))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("dial failed: {0}")]
    DialFailed(#[source] io::Error),

    #[error("unsupported dial as listener")]
    UnsupportedDialAsListener,

    #[error("listen failed: {0}")]
    ListenFailed(#[source] io::Error),
}

impl From<Error> for TransportError<Error> {
    fn from(value: Error) -> Self {
        TransportError::Other(value)
    }
}
