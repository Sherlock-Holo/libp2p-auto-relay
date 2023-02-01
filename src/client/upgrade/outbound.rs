use std::future::Future;
use std::io;

use asynchronous_codec::Framed;
use futures_util::future::Either;
use futures_util::{SinkExt, TryStreamExt};
use libp2p_core::{Multiaddr, PeerId, UpgradeInfo};
use libp2p_swarm::NegotiatedSubstream;
use tap::TapFallible;
use tracing::{debug, debug_span, error, instrument, Instrument};

use super::{UpgradeAction, UpgradeError, AUTO_RELAY_PROTOCOL, MAX_MESSAGE_SIZE};
use crate::client::connection::Connection;
use crate::pb;

#[derive(Debug)]
pub enum OutboundUpgrade {
    Dial {
        dst_addr: Multiaddr,
    },
    Listen {
        listen_peer_id: PeerId,
        listen_addr: Multiaddr,
    },
}

impl OutboundUpgrade {
    pub fn new_dial(dst_addr: Multiaddr) -> Self {
        Self::Dial { dst_addr }
    }

    pub fn new_listen(listen_peer_id: PeerId, listen_addr: Multiaddr) -> Self {
        Self::Listen {
            listen_peer_id,
            listen_addr,
        }
    }

    #[instrument(level = "debug", err)]
    async fn dial_upgrade(
        dst_addr: Multiaddr,
        socket: NegotiatedSubstream,
    ) -> Result<Connection, UpgradeError> {
        let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));
        let dial_request = pb::DialRequest {
            addr: dst_addr.to_string(),
        };

        framed.send(dial_request).await.map_err(|err| {
            error!(%err, "send dial request failed");

            io::Error::from(err)
        })?;

        debug!("send dial request done");

        let pb::DialResponse {
            result: dial_result,
        } = match framed.try_next().await.map_err(|err| {
            error!(%err, "receive dial response failed");

            io::Error::from(err)
        })? {
            None => {
                error!("unexpected stream closed");

                return Err(UpgradeError::UnexpectedStreamClosed);
            }

            Some(resp) => resp,
        };

        debug!("receive dial response done");

        match dial_result {
            None => {
                error!("no dial result");

                return Err(UpgradeError::UnexpectedResult {
                    action: UpgradeAction::Dial,
                });
            }

            Some(pb::dial_response::Result::Failed(pb::DialFailedResponse { reason })) => {
                error!(%reason, "dial failed");

                return Err(UpgradeError::ActionFailed {
                    action: UpgradeAction::Dial,
                    reason: Some(reason),
                });
            }

            Some(pb::dial_response::Result::Success(pb::DialSuccessResponse {})) => {
                debug!("dial done");
            }
        }

        let framed_parts = framed.into_parts();

        Ok(Connection::new(
            dst_addr,
            framed_parts.read_buffer.freeze(),
            framed_parts.io,
        ))
    }

    #[instrument(level = "debug", err)]
    async fn listen_upgrade(
        listen_peer_id: PeerId,
        listen_addr: Multiaddr,
        socket: NegotiatedSubstream,
    ) -> Result<(), UpgradeError> {
        let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));
        let listen_request = pb::ListenRequest {
            local_peer_id: listen_peer_id.to_string(),
            local_addr: listen_addr.to_string(),
        };

        framed.send(listen_request).await.tap_err(|err| {
            error!(%err, "send listen request failed");
        })?;

        debug!("send listen request done");

        let pb::ListenResponse { result } = match framed.try_next().await.tap_err(|err| {
            error!(%err, "receive listen response failed");
        })? {
            None => {
                error!("unexpected stream closed");

                return Err(UpgradeError::UnexpectedStreamClosed);
            }

            Some(resp) => resp,
        };

        debug!("receive listen response done");

        match result {
            None => {
                error!("no listen result");

                return Err(UpgradeError::UnexpectedResult {
                    action: UpgradeAction::Listen,
                });
            }

            Some(pb::listen_response::Result::Failed(pb::ListenFailedResponse { reason })) => {
                error!(%reason, "listen failed");

                return Err(UpgradeError::ActionFailed {
                    action: UpgradeAction::Listen,
                    reason: Some(reason),
                });
            }

            Some(pb::listen_response::Result::Success(_)) => {
                debug!("listen done");
            }
        }

        Ok(())
    }
}

impl UpgradeInfo for OutboundUpgrade {
    type Info = &'static [u8];
    type InfoIter = [Self::Info; 1];

    fn protocol_info(&self) -> Self::InfoIter {
        [AUTO_RELAY_PROTOCOL]
    }
}

impl libp2p_core::OutboundUpgrade<NegotiatedSubstream> for OutboundUpgrade {
    type Output = Either<Connection, ()>;
    type Error = UpgradeError;
    type Future = impl Future<Output = Result<Self::Output, Self::Error>>;

    #[instrument(level = "debug")]
    fn upgrade_outbound(self, socket: NegotiatedSubstream, _info: Self::Info) -> Self::Future {
        async move {
            match self {
                OutboundUpgrade::Dial { dst_addr } => {
                    Ok(Either::Left(Self::dial_upgrade(dst_addr, socket).await?))
                }

                OutboundUpgrade::Listen {
                    listen_peer_id,
                    listen_addr,
                } => Ok(Either::Right(
                    Self::listen_upgrade(listen_peer_id, listen_addr, socket).await?,
                )),
            }
        }
        .instrument(debug_span!("upgrade_outbound"))
    }
}
