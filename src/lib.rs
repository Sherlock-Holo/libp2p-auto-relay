#![feature(type_alias_impl_trait)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(feature = "combine")]
pub mod combine;
mod connection;
pub mod endpoint;
pub mod relay;

#[allow(clippy::derive_partial_eq_without_eq)]
mod pb {
    include!(concat!(env!("OUT_DIR"), "/auto_relay.rs"));
}

pub(crate) const AUTO_RELAY_DIAL_PROTOCOL: &[u8] = b"/auto-relay/0.1/dial";
pub(crate) const AUTO_RELAY_LISTEN_PROTOCOL: &[u8] = b"/auto-relay/0.1/listen";
pub(crate) const AUTO_RELAY_CONNECT_PROTOCOL: &[u8] = b"/auto-relay/0.1/connect";
pub(crate) const MAX_MESSAGE_SIZE: usize = 4096;
