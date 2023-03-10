use std::future::Future;
use std::io;

use asynchronous_codec::Framed;
use futures_util::{Sink, SinkExt, TryStreamExt};
use libp2p_core::{Multiaddr, PeerId, UpgradeInfo};
use libp2p_swarm::NegotiatedSubstream;
use tracing::{debug, debug_span, error, instrument, warn, Instrument};

use super::UpgradeError;
use crate::connection::Connection;
use crate::endpoint::upgrade::UpgradeAction;
use crate::{pb, AUTO_RELAY_CONNECT_PROTOCOL, MAX_MESSAGE_SIZE};

#[derive(Debug)]
pub struct InboundUpgrade {
    local_peer_id: PeerId,
    relay_peer_id: Option<PeerId>,
}

impl InboundUpgrade {
    pub fn new(local_peer_id: PeerId, relay_peer_id: Option<PeerId>) -> Self {
        Self {
            local_peer_id,
            relay_peer_id,
        }
    }
}

impl UpgradeInfo for InboundUpgrade {
    type Info = &'static [u8];
    type InfoIter = [Self::Info; 1];

    fn protocol_info(&self) -> Self::InfoIter {
        [AUTO_RELAY_CONNECT_PROTOCOL]
    }
}

impl libp2p_core::InboundUpgrade<NegotiatedSubstream> for InboundUpgrade {
    type Output = InboundUpgradeOutput;
    type Error = UpgradeError;
    type Future = impl Future<Output = Result<Self::Output, Self::Error>>;

    #[instrument(level = "debug")]
    fn upgrade_inbound(self, socket: NegotiatedSubstream, _info: Self::Info) -> Self::Future {
        async move {
            let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));

            let pb::ConnectRequest {
                dst_addr,
                dst_peer_id,
                dialer_addr,
            } = match framed.try_next().await.map_err(|err| {
                error!(%err, "receive connect request failed");

                io::Error::from(err)
            })? {
                None => {
                    error!("unexpected stream closed");

                    return Err(UpgradeError::UnexpectedStreamClosed);
                }

                Some(req) => req,
            };

            debug!(%dst_addr, %dialer_addr, "receive connect request done");

            let dst_addr = parse_addr("dst_addr", dst_addr, &mut framed).await?;

            debug!(%dst_addr, "parse dst addr done");

            let dst_peer_id = parse_peer_id(dst_peer_id, &mut framed).await?;

            debug!(%dst_peer_id, "parse dst peer id");

            if self.local_peer_id != dst_peer_id {
                error!(%dst_addr, "not listen addr");

                let _ = framed
                    .send(pb::ConnectResponse {
                        result: Some(pb::connect_response::Result::Failed(
                            pb::ConnectFailedResponse {
                                reason: format!("dst addr {dst_addr} not listen"),
                            },
                        )),
                    })
                    .await;

                return Err(UpgradeError::ActionFailed {
                    action: UpgradeAction::Connect,
                    reason: Some(format!("dst addr {dst_addr} not listen")),
                });
            }

            let dialer_addr = parse_addr("dialer_addr", dialer_addr, &mut framed).await?;

            debug!(%dialer_addr, "parse dialer addr done");

            if let Err(err) = framed
                .send(pb::ConnectResponse {
                    result: Some(pb::connect_response::Result::Success(
                        pb::ConnectSuccessResponse {},
                    )),
                })
                .await
            {
                error!(%err, "send connect success response failed");

                return Err(err.into());
            }

            debug!(%dialer_addr, "send connect success response done");

            if let Err(err) = framed.flush().await {
                error!(%err, %dialer_addr, "flush framed failed");

                return Err(err.into());
            }

            debug!(%dialer_addr, "flush framed done");

            let framed_parts = framed.into_parts();

            Ok(InboundUpgradeOutput {
                listen_peer_id: self.local_peer_id,
                listen_addr: dst_addr,
                relay_peer_id: self.relay_peer_id.unwrap(),
                connection: Connection::new(
                    dialer_addr,
                    framed_parts.read_buffer.freeze(),
                    framed_parts.io,
                ),
            })
        }
        .instrument(debug_span!("upgrade_inbound"))
    }
}

#[instrument(level = "debug", err, skip(framed))]
async fn parse_addr<S>(
    addr_type: &str,
    addr: String,
    framed: &mut S,
) -> Result<Multiaddr, UpgradeError>
where
    S: Sink<pb::ConnectResponse> + Unpin,
    S::Error: std::error::Error,
{
    match addr.parse::<Multiaddr>().map_err(|err| {
        error!(%err, %addr, "invalid {addr_type} addr");

        UpgradeError::InvalidAddr(addr)
    }) {
        Err(err) => {
            if let Err(send_err) = framed
                .send(pb::ConnectResponse {
                    result: Some(pb::connect_response::Result::Failed(
                        pb::ConnectFailedResponse {
                            reason: err.to_string(),
                        },
                    )),
                })
                .await
            {
                warn!(%send_err, "send connect failed response failed");
            }

            Err(err)
        }

        Ok(addr) => Ok(addr),
    }
}

#[instrument(level = "debug", skip(framed))]
async fn parse_peer_id<S>(peer_id: String, framed: &mut S) -> Result<PeerId, UpgradeError>
where
    S: Sink<pb::ConnectResponse> + Unpin,
    S::Error: std::error::Error,
{
    match peer_id.parse::<PeerId>().map_err(|err| {
        error!(%err, "invalid peer id");

        UpgradeError::InvalidPeerId(peer_id)
    }) {
        Err(err) => {
            if let Err(send_err) = framed
                .send(pb::ConnectResponse {
                    result: Some(pb::connect_response::Result::Failed(
                        pb::ConnectFailedResponse {
                            reason: err.to_string(),
                        },
                    )),
                })
                .await
            {
                warn!(%send_err, "send connect failed response failed");
            }

            Err(err)
        }

        Ok(addr) => Ok(addr),
    }
}

#[derive(Debug)]
pub struct InboundUpgradeOutput {
    pub listen_peer_id: PeerId,
    pub listen_addr: Multiaddr,
    pub relay_peer_id: PeerId,
    pub connection: Connection,
}
