use std::future::Future;

use asynchronous_codec::Framed;
use futures_util::{SinkExt, TryStreamExt};
use libp2p_core::{Multiaddr, PeerId, UpgradeInfo};
use libp2p_swarm::NegotiatedSubstream;
use tracing::{debug, debug_span, error, instrument, Instrument};

use super::{UpgradeAction, UpgradeError};
use crate::connection::Connection;
use crate::{pb, AUTO_RELAY_CONNECT_PROTOCOL, MAX_MESSAGE_SIZE};

#[derive(Debug)]
pub struct OutboundUpgrade {
    dialer_addr: Multiaddr,
    dst_peer_id: PeerId,
    dst_addr: Multiaddr,
}

impl OutboundUpgrade {
    pub fn new(dialer_addr: Multiaddr, dst_peer_id: PeerId, dst_addr: Multiaddr) -> Self {
        Self {
            dialer_addr,
            dst_peer_id,
            dst_addr,
        }
    }
}

impl UpgradeInfo for OutboundUpgrade {
    type Info = &'static [u8];
    type InfoIter = [Self::Info; 1];

    fn protocol_info(&self) -> Self::InfoIter {
        [AUTO_RELAY_CONNECT_PROTOCOL]
    }
}

impl libp2p_core::OutboundUpgrade<NegotiatedSubstream> for OutboundUpgrade {
    type Output = Connection;
    type Error = UpgradeError;
    type Future = impl Future<Output = Result<Self::Output, Self::Error>>;

    #[instrument(level = "debug")]
    fn upgrade_outbound(self, socket: NegotiatedSubstream, _info: Self::Info) -> Self::Future {
        async move {
            let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));
            framed.send(pb::ConnectRequest {
                dst_addr: self.dst_addr.to_string(),
                dst_peer_id: self.dst_peer_id.to_string(),
                dialer_addr: self.dialer_addr.to_string(),
            }).await
                .map_err(|err| {
                    error!(dst_addr = %self.dst_addr, dialer_addr = %self.dialer_addr, %err, "send connect request failed");

                    UpgradeError::UnexpectedStreamClosed
                })?;

            let result = match framed.try_next().await.map_err(|err| {
                error!(dst_addr = %self.dst_addr, dialer_addr = %self.dialer_addr, %err, "receive connect response failed");

                UpgradeError::UnexpectedStreamClosed
            })? {
                None => {
                    error!(dst_addr = %self.dst_addr, "unexpected stream closed");

                    return Err(UpgradeError::UnexpectedStreamClosed);
                }

                Some(pb::ConnectResponse { result: None }) => {
                    error!(dst_addr = %self.dst_addr, "unexpected result");

                    return Err(UpgradeError::UnexpectedResult);
                }

                Some(pb::ConnectResponse { result: Some(result) }) => result
            };

            debug!("receive connect response done");

            match result {
                pb::connect_response::Result::Failed(pb::ConnectFailedResponse { reason }) => {
                    error!(dst_addr = %self.dst_addr, %reason, "connect failed");

                    return Err(UpgradeError::ActionFailed {
                        action: UpgradeAction::Connect,
                        reason: Some(reason),
                    });
                }

                pb::connect_response::Result::Success(pb::ConnectSuccessResponse {}) => {}
            }

            debug!(dst_addr = %self.dst_addr, "connect done");

            let framed_parts = framed.into_parts();

            Ok(Connection::new(
                self.dst_addr,
                framed_parts.read_buffer.freeze(),
                framed_parts.io,
            ))
        }.instrument(debug_span!("upgrade_outbound"))
    }
}
