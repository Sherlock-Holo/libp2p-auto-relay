use std::io;

pub use inbound::{InboundUpgrade, InboundUpgradeOutput};
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

    #[error("unexpected result")]
    UnexpectedResult { action: UpgradeAction },

    #[error("action failed: {reason:?}")]
    ActionFailed {
        action: UpgradeAction,
        reason: Option<String>,
    },

    #[error("invalid addr: {0}")]
    InvalidAddr(String),
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
