use std::io;

pub use inbound::{ConnectRequest, InboundUpgrade, ListenRequest};
use libp2p_core::Multiaddr;
pub use outbound::OutboundUpgrade;
use thiserror::Error;

mod inbound;
mod outbound;

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("io error: {0}")]
    Io(
        #[from]
        #[source]
        io::Error,
    ),

    #[error("unexpected stream closed")]
    UnexpectedStreamClosed,

    #[error("behaviour is dropped")]
    BehaviourDropped,

    #[error("behaviour connect {addr} failed: {err}")]
    ConnectFailed {
        addr: Multiaddr,
        #[source]
        err: io::Error,
    },

    #[error("dial dst peer id {0} invalid")]
    InvalidDstPeerId(String),

    #[error("dial dst addr {0} invalid")]
    InvalidDstAddr(String),

    #[error("invalid listen peer id {0}")]
    InvalidListenPeerId(String),

    #[error("invalid listen addr {0}")]
    InvalidListenAddr(String),

    #[error("unexpected result")]
    UnexpectedResult,

    #[error("action failed: {reason:?}")]
    ActionFailed {
        action: UpgradeAction,
        reason: Option<String>,
    },
}

impl From<prost_codec::Error> for UpgradeError {
    fn from(value: prost_codec::Error) -> Self {
        Self::Io(value.into())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum UpgradeAction {
    Dial,
    Listen,
    Connect,
}
