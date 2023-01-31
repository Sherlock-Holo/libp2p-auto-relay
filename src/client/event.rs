use std::io;

use futures_channel::{mpsc, oneshot};
use libp2p_core::transport::ListenerId;
use libp2p_core::{Multiaddr, PeerId};
use thiserror::Error;

use super::connection::Connection;

#[derive(Debug)]
pub enum TransportToBehaviourEvent {
    Dial {
        dst_addr: Multiaddr,
        relay_addr: Multiaddr,
        connection_sender: oneshot::Sender<io::Result<Connection>>,
    },

    Listen {
        listener_id: ListenerId,
        local_peer_id: PeerId,
        local_addr: Multiaddr,
        relay_peer_id: PeerId,
        relay_addr: Multiaddr,
        connection_sender: mpsc::Sender<io::Result<Connection>>,
    },
}

#[derive(Debug)]
pub enum BehaviourToTransportEvent {
    ListenSuccess {
        listener_id: ListenerId,
        local_addr: Multiaddr,
    },

    ListenFailed {
        err: io::Error,
        listener_id: ListenerId,
        local_addr: Multiaddr,
    },

    PeerClosed {
        peer_id: PeerId,
        peer_addr: Multiaddr,
    },
}

#[derive(Debug)]
pub enum Event {
    UnexpectedConnection {
        listen_addr: Multiaddr,
    },

    UnexpectedListenerClosed {
        listen_addr: Multiaddr,
        err: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    OtherError(Box<dyn std::error::Error + Send + Sync + 'static>),

    UnexpectedTransportDropped,
}

#[derive(Debug)]
pub enum ConnectionHandlerInEvent {
    Dial {
        dst_addr: Multiaddr,
        connection_sender: oneshot::Sender<io::Result<Connection>>,
    },

    Listen {
        listener_id: ListenerId,
        local_peer_id: PeerId,
        local_addr: Multiaddr,
    },
}

#[derive(Debug)]
pub enum ConnectionHandlerOutEvent {
    NewConnection {
        listen_addr: Multiaddr,
        connection: Connection,
    },

    DialSuccess {
        connection: Connection,
        sender: oneshot::Sender<io::Result<Connection>>,
    },

    DialFailed {
        err: io::Error,
        sender: oneshot::Sender<io::Result<Connection>>,
    },

    ListenSuccess {
        listener_id: ListenerId,
        local_peer_id: PeerId,
        listen_addr: Multiaddr,
    },

    ListenFailed {
        err: io::Error,
        listener_id: ListenerId,
        local_peer_id: PeerId,
        listen_addr: Multiaddr,
    },

    Error(Box<dyn std::error::Error + Send + Sync + 'static>),
}

#[derive(Debug)]
pub enum OutboundOpenInfo {
    Dial {
        connection_sender: oneshot::Sender<io::Result<Connection>>,
    },

    Listen {
        listener_id: ListenerId,
        local_peer_id: PeerId,
        listen_addr: Multiaddr,
    },
}

#[derive(Debug, Error)]
pub enum ConnectionHandlerError {}