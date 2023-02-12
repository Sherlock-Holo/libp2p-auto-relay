use std::future::Future;
use std::io;

use asynchronous_codec::Framed;
use futures_channel::{mpsc, oneshot};
use futures_util::future::Either;
use futures_util::{SinkExt, TryStreamExt};
use libp2p_core::{Multiaddr, PeerId, UpgradeInfo};
use libp2p_swarm::NegotiatedSubstream;
use tap::TapFallible;
use tracing::{debug, debug_span, error, instrument, Instrument};

use super::UpgradeError;
use crate::connection::Connection;
use crate::{pb, AUTO_RELAY_DIAL_PROTOCOL, AUTO_RELAY_LISTEN_PROTOCOL, MAX_MESSAGE_SIZE};

#[derive(Debug)]
pub struct InboundUpgrade {
    remote_addr: Option<Multiaddr>,
    connect_request_sender: mpsc::Sender<ConnectRequest>,
}

impl InboundUpgrade {
    pub fn new(
        remote_addr: Option<Multiaddr>,
        connect_request_sender: mpsc::Sender<ConnectRequest>,
    ) -> Self {
        Self {
            remote_addr,
            connect_request_sender,
        }
    }
}

impl UpgradeInfo for InboundUpgrade {
    type Info = &'static [u8];
    type InfoIter = [Self::Info; 2];

    fn protocol_info(&self) -> Self::InfoIter {
        [AUTO_RELAY_DIAL_PROTOCOL, AUTO_RELAY_LISTEN_PROTOCOL]
    }
}

impl InboundUpgrade {
    #[instrument(level = "debug")]
    async fn dial_upgrade(
        mut self,
        socket: NegotiatedSubstream,
    ) -> Result<(Connection, Connection), UpgradeError> {
        let dialer_addr = self.remote_addr.expect("remote addr is None");
        let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));
        let pb::DialRequest { peer_id, addr } = match framed
            .try_next()
            .await
            .tap_err(|err| error!(%err, "receive dial request failed"))?
        {
            None => {
                error!("unexpected stream closed");

                return Err(UpgradeError::UnexpectedStreamClosed);
            }

            Some(req) => req,
        };

        debug!(%peer_id, %addr, "receive dial request done");

        let peer_id = match peer_id.parse::<PeerId>() {
            Err(err) => {
                error!(%err, "peer id invalid");

                let _ = framed
                    .send(pb::DialResponse {
                        result: Some(pb::dial_response::Result::Failed(pb::DialFailedResponse {
                            reason: err.to_string(),
                        })),
                    })
                    .await;

                return Err(UpgradeError::InvalidDstPeerId(peer_id));
            }

            Ok(peer_id) => peer_id,
        };

        debug!(%peer_id, "parse dst peer id done");

        let addr = match addr.parse::<Multiaddr>() {
            Err(err) => {
                error!(%err, "dst addr invalid");

                let _ = framed
                    .send(pb::DialResponse {
                        result: Some(pb::dial_response::Result::Failed(pb::DialFailedResponse {
                            reason: err.to_string(),
                        })),
                    })
                    .await;

                return Err(UpgradeError::InvalidDstAddr(addr));
            }

            Ok(addr) => addr,
        };

        debug!(%addr, "parse dst addr done");

        let (connection_sender, connection_receiver) = oneshot::channel();

        let connect_request = ConnectRequest {
            dst_peer_id: peer_id,
            dst_addr: addr.clone(),
            dialer_addr: dialer_addr.clone(),
            connection_sender,
        };

        self.connect_request_sender
            .send(connect_request)
            .await
            .map_err(|err| {
                error!(%err, %addr, "behaviour is dropped");

                UpgradeError::BehaviourDropped
            })?;

        debug!(%addr, "send connect request to behaviour done");

        let connection = match connection_receiver.await.map_err(|err| {
            error!(%err, %addr, "behaviour is dropped");

            UpgradeError::BehaviourDropped
        })? {
            Err(err) => {
                error!(%err, %addr, "behaviour connect failed");

                let _ = framed
                    .send(pb::DialResponse {
                        result: Some(pb::dial_response::Result::Failed(pb::DialFailedResponse {
                            reason: err.to_string(),
                        })),
                    })
                    .await;

                return Err(UpgradeError::ConnectFailed { addr, err });
            }

            Ok(connection) => connection,
        };

        debug!(%addr, "connect done");

        if let Err(err) = framed
            .send(pb::DialResponse {
                result: Some(pb::dial_response::Result::Success(
                    pb::DialSuccessResponse {},
                )),
            })
            .await
        {
            error!(%err, %addr, "send dial success response failed");

            return Err(UpgradeError::UnexpectedStreamClosed);
        }

        debug!(%addr, "send dial success response done");

        let framed_parts = framed.into_parts();
        let dialer_connection = Connection::new(
            dialer_addr,
            framed_parts.read_buffer.freeze(),
            framed_parts.io,
        );

        Ok((dialer_connection, connection))
    }

    #[instrument(level = "debug")]
    async fn listen_upgrade(
        self,
        socket: NegotiatedSubstream,
    ) -> Result<ListenRequest, UpgradeError> {
        let mut framed = Framed::new(socket, prost_codec::Codec::new(MAX_MESSAGE_SIZE));
        let pb::ListenRequest {
            listen_peer_id,
            listen_addr,
        } = match framed
            .try_next()
            .await
            .tap_err(|err| error!(%err, "receive listen request failed"))?
        {
            None => {
                error!("unexpected stream closed");

                return Err(UpgradeError::UnexpectedStreamClosed);
            }

            Some(req) => req,
        };

        debug!(%listen_peer_id, %listen_addr, "receive listen request done");

        let listen_peer_id = match listen_peer_id.parse::<PeerId>() {
            Err(err) => {
                error!(%err, %listen_peer_id, "parse listen peer id failed");

                let _ = framed
                    .send(pb::ListenResponse {
                        result: Some(pb::listen_response::Result::Failed(
                            pb::ListenFailedResponse {
                                reason: err.to_string(),
                            },
                        )),
                    })
                    .await;

                return Err(UpgradeError::InvalidListenPeerId(listen_peer_id));
            }

            Ok(listen_peer_id) => listen_peer_id,
        };

        debug!(%listen_peer_id, "parse listen peer id done");

        let listen_addr = match listen_addr.parse::<Multiaddr>() {
            Err(err) => {
                error!(%err, "parse listen addr failed");

                let _ = framed
                    .send(pb::ListenResponse {
                        result: Some(pb::listen_response::Result::Failed(
                            pb::ListenFailedResponse {
                                reason: err.to_string(),
                            },
                        )),
                    })
                    .await;

                return Err(UpgradeError::InvalidListenAddr(listen_addr));
            }

            Ok(listen_addr) => listen_addr,
        };

        debug!(%listen_addr, "parse listen addr done");

        framed
            .send(pb::ListenResponse {
                result: Some(pb::listen_response::Result::Success(
                    pb::ListenSuccessResponse {},
                )),
            })
            .await
            .tap_err(|err| {
                error!(%listen_peer_id, %listen_addr, %err, "send listen success response failed");
            })?;

        debug!(%listen_peer_id, %listen_addr, "send listen success response done");

        Ok(ListenRequest {
            listen_peer_id,
            listen_addr,
        })
    }
}

impl libp2p_core::InboundUpgrade<NegotiatedSubstream> for InboundUpgrade {
    type Output = Either<(Connection, Connection), ListenRequest>;
    type Error = UpgradeError;
    type Future = impl Future<Output = Result<Self::Output, Self::Error>>;

    #[instrument(level = "debug")]
    fn upgrade_inbound(self, socket: NegotiatedSubstream, info: Self::Info) -> Self::Future {
        async move {
            Ok(match info {
                AUTO_RELAY_DIAL_PROTOCOL => Either::Left(self.dial_upgrade(socket).await?),
                AUTO_RELAY_LISTEN_PROTOCOL => Either::Right(self.listen_upgrade(socket).await?),
                _ => unreachable!(),
            })
        }
        .instrument(debug_span!("upgrade_inbound"))
    }
}

#[derive(Debug)]
pub struct ConnectRequest {
    pub dst_peer_id: PeerId,
    pub dst_addr: Multiaddr,
    pub dialer_addr: Multiaddr,
    pub connection_sender: oneshot::Sender<io::Result<Connection>>,
}

#[derive(Debug)]
pub struct ListenRequest {
    pub listen_peer_id: PeerId,
    pub listen_addr: Multiaddr,
}
