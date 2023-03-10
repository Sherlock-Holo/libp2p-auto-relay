use std::io;

use libp2p_core::{Multiaddr, PeerId};

use crate::connection::Connection;

/// the relay server behaviour event
#[derive(Debug)]
pub enum Event {
    /// start forward data
    StartCopy {
        dialer_addr: Multiaddr,
        dst_addr: Multiaddr,
    },

    /// a new endpoint listens on a addr
    Listen {
        listen_peer_id: PeerId,
        listen_addr: Multiaddr,
    },

    /// listen or dial failed
    ListenOrDialFailed { err: io::Error },

    /// invalid connect request
    ConnectToNotListenPeer {
        dialer_addr: Multiaddr,
        dst_peer_id: PeerId,
    },
}

#[derive(Debug)]
pub enum ConnectionHandlerOutEvent {
    DialSuccess {
        dialer_connection: Connection,
        connection: Connection,
    },

    ConnectSuccess {
        connection: Connection,
        dst_addr: Multiaddr,
        dst_peer_id: PeerId,
    },

    ConnectFailed {
        err: io::Error,
        dst_addr: Multiaddr,
        dst_peer_id: PeerId,
    },

    ListenSuccess {
        listen_peer_id: PeerId,
        listen_addr: Multiaddr,
    },

    ListenOrDialFailed {
        err: io::Error,
    },
}

#[derive(Debug)]
pub enum ConnectionHandlerInEvent {
    Connect {
        dst_addr: Multiaddr,
        dst_peer_id: PeerId,
        dialer_addr: Multiaddr,
    },
}
