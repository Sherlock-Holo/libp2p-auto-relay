use std::io;

use futures_channel::{mpsc, oneshot};
use libp2p_core::transport::ListenerId;
use libp2p_core::{Multiaddr, PeerId};

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
        /*relay_peer_id: PeerId,
        relay_addr: Multiaddr,*/
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
