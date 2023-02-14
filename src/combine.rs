//! help user combine 2 transports

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{FutureExt, TryFutureExt};
use libp2p_core::either::{EitherError, EitherFuture, EitherOutput};
use libp2p_core::transport::{ListenerId, TransportError, TransportEvent};
use libp2p_core::{Multiaddr, Transport};

/// a combine transport
///
/// when using the endpoint transport, we hope the swarm listen an addr on the normal transport and
/// the [`endpoint-transport`](super::endpoint::Transport), currently the
/// [`Transport::or_transport`] can't do this job, also we hope dial an addr can dial through the
/// normal transport and the [`endpoint-transport`](super::endpoint::Transport), return the first
/// successfully connection. This combine transport can help us do this job
#[derive(Debug)]
#[pin_project::pin_project]
pub struct CombineTransport<A, B> {
    #[pin]
    t1: A,
    #[pin]
    t2: B,
    listener_ids: HashMap<ListenerId, (ListenerId, ListenerId)>,
}

impl<A, B> CombineTransport<A, B> {
    /// create a combine transport with 2 transports
    pub fn new(a: A, b: B) -> CombineTransport<A, B> {
        CombineTransport {
            t1: a,
            t2: b,
            listener_ids: Default::default(),
        }
    }
}

impl<A, B> CombineTransport<A, B>
where
    A: Transport,
    B: Transport,
{
    fn inner_dial(
        &mut self,
        addr: Multiaddr,
        as_listener: bool,
    ) -> Result<
        impl Future<
            Output = Result<EitherOutput<A::Output, B::Output>, EitherError<A::Error, B::Error>>,
        >,
        TransportError<EitherError<A::Error, B::Error>>,
    > {
        let dial1;
        let dial2;

        if !as_listener {
            dial1 = self.t1.dial(addr.clone());
            dial2 = self.t2.dial(addr);
        } else {
            dial1 = self.t1.dial_as_listener(addr.clone());
            dial2 = self.t2.dial_as_listener(addr);
        }

        let (dial1, dial2) = match (dial1, dial2) {
            (Err(_), Err(err)) => return Err(err.map(EitherError::B)),
            (dial1, dial2) => (dial1, dial2),
        };

        Ok(async move {
            match (dial1, dial2) {
                (Err(_), Err(_)) => unreachable!(),

                (Ok(dial), Err(_)) => {
                    dial.map_err(EitherError::A)
                        .map_ok(EitherOutput::First)
                        .await
                }
                (Err(_), Ok(dial)) => {
                    dial.map_err(EitherError::B)
                        .map_ok(EitherOutput::Second)
                        .await
                }
                (Ok(dial1), Ok(dial2)) => {
                    let dial1 = dial1.map_err(EitherError::A).map_ok(EitherOutput::First);
                    let dial2 = dial2.map_err(EitherError::B).map_ok(EitherOutput::Second);

                    futures_util::pin_mut!(dial1);
                    futures_util::pin_mut!(dial2);
                    let mut dial1_mut = (&mut dial1).fuse();
                    let mut dial2_mut = (&mut dial2).fuse();

                    futures_util::select! {
                        r1 = dial1_mut => {
                            match r1 {
                                Ok(r1) => Ok(r1),
                                Err(_) => dial2.await
                            }
                        }

                        r2 = dial2_mut => {
                            match r2 {
                                Ok(r2) => Ok(r2),
                                Err(_) => dial1.await
                            }
                        }
                    }
                }
            }
        })
    }
}

impl<A, B> Transport for CombineTransport<A, B>
where
    B: Transport,
    A: Transport,
{
    type Output = EitherOutput<A::Output, B::Output>;
    type Error = EitherError<A::Error, B::Error>;
    type ListenerUpgrade = EitherFuture<A::ListenerUpgrade, B::ListenerUpgrade>;
    type Dial = impl Future<Output = Result<Self::Output, Self::Error>>;

    fn listen_on(&mut self, addr: Multiaddr) -> Result<ListenerId, TransportError<Self::Error>> {
        let id1 = self.t1.listen_on(addr.clone()).map_err(|err| match err {
            TransportError::MultiaddrNotSupported(addr) => {
                TransportError::MultiaddrNotSupported(addr)
            }
            TransportError::Other(err) => TransportError::Other(EitherError::A(err)),
        })?;
        let id2 = self.t2.listen_on(addr).map_err(|err| match err {
            TransportError::MultiaddrNotSupported(addr) => {
                TransportError::MultiaddrNotSupported(addr)
            }
            TransportError::Other(err) => TransportError::Other(EitherError::B(err)),
        })?;

        let listener_id = ListenerId::new();
        self.listener_ids.insert(listener_id, (id1, id2));

        Ok(listener_id)
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        if let Some((id1, id2)) = self.listener_ids.remove(&id) {
            self.t1.remove_listener(id1) || self.t2.remove_listener(id2)
        } else {
            false
        }
    }

    fn dial(&mut self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.inner_dial(addr, false)
    }

    fn dial_as_listener(
        &mut self,
        addr: Multiaddr,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.inner_dial(addr, true)
    }

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        let this = self.project();
        match this.t1.poll(cx) {
            Poll::Ready(ev) => {
                return Poll::Ready(ev.map_upgrade(EitherFuture::First).map_err(EitherError::A))
            }
            Poll::Pending => {}
        }
        match this.t2.poll(cx) {
            Poll::Ready(ev) => {
                return Poll::Ready(ev.map_upgrade(EitherFuture::Second).map_err(EitherError::B))
            }
            Poll::Pending => {}
        }
        Poll::Pending
    }

    fn address_translation(&self, server: &Multiaddr, observed: &Multiaddr) -> Option<Multiaddr> {
        self.t1
            .address_translation(server, observed)
            .or_else(|| self.t2.address_translation(server, observed))
    }
}
