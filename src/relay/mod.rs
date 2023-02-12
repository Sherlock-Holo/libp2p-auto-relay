//! make the p2p node as a relay server, help endpoints communicate to each other which can't
//! create a TCP directly to other endpoint

pub use self::behaviour::Behaviour;
pub use self::event::Event;

mod behaviour;
mod event;
mod handler;
mod upgrade;
