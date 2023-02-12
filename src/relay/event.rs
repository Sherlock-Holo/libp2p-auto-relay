use std::io;

use libp2p_core::{Multiaddr, PeerId};

use crate::connection::Connection;

#[derive(Debug)]
pub enum Event {
    StartCopy {
        dialer_addr: Multiaddr,
        dst_addr: Multiaddr,
    },

    Listen {
        listen_peer_id: PeerId,
        listen_addr: Multiaddr,
    },

    ListenOrDialFailed {
        err: io::Error,
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
