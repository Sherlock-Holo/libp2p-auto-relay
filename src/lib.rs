#![feature(type_alias_impl_trait)]

pub mod client;

#[allow(clippy::derive_partial_eq_without_eq)]
mod pb {
    include!(concat!(env!("OUT_DIR"), "/auto_relay.rs"));
}
