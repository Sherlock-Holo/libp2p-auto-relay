use std::future::Future;
use std::io;
use std::sync::Arc;

use asynchronous_codec::Framed;
use dashmap::DashSet;
use futures_util::{Sink, SinkExt, TryStreamExt};
use libp2p_core::{Multiaddr, UpgradeInfo};
use libp2p_swarm::NegotiatedSubstream;
use tracing::{debug, debug_span, error, instrument, warn, Instrument};

use super::UpgradeError;
use crate::client::upgrade::UpgradeAction;
use crate::connection::Connection;
use crate::{pb, AUTO_RELAY_CONNECT_PROTOCOL, MAX_MESSAGE_SIZE};

#[derive(Debug)]
pub struct InboundUpgrade {
    listen_addrs: Arc<DashSet<Multiaddr>>,
}

impl InboundUpgrade {
    pub fn new(listen_addrs: Arc<DashSet<Multiaddr>>) -> Self {
        Self { listen_addrs }
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

            if !self.listen_addrs.contains(&dst_addr) {
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
                listen_addr: dst_addr,
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

#[derive(Debug)]
pub struct InboundUpgradeOutput {
    pub listen_addr: Multiaddr,
    pub connection: Connection,
}
