//! help user combine 2 transports

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use futures_util::{future, FutureExt, TryFutureExt};
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

type _DialResult<AO, BO, AE, BE> = Result<
    BoxFuture<'static, Result<EitherOutput<AO, BO>, EitherError<AE, BE>>>,
    TransportError<EitherError<AE, BE>>,
>;

impl<A, B> CombineTransport<A, B>
where
    A: Transport,
    B: Transport,
    A::Output: Send + 'static,
    B::Output: Send + 'static,
    A::Error: Send + 'static,
    B::Error: Send + 'static,
    A::Dial: Send + 'static,
    B::Dial: Send + 'static,
{
    fn _dial(
        &mut self,
        addr: Multiaddr,
        as_listener: bool,
    ) -> _DialResult<A::Output, B::Output, A::Error, B::Error> {
        let dial1;
        let dial2;

        if !as_listener {
            dial1 = self.t1.dial(addr.clone());
            dial2 = self.t2.dial(addr);
        } else {
            dial1 = self.t1.dial_as_listener(addr.clone());
            dial2 = self.t2.dial_as_listener(addr);
        }

        match (dial1, dial2) {
            (Err(_), Err(err)) => Err(err.map(EitherError::B)),
            (Ok(dial), Err(_)) => {
                let dial = dial
                    .map_err(EitherError::A)
                    .map_ok(EitherOutput::First)
                    .boxed();
                Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
            }
            (Err(_), Ok(dial)) => {
                let dial = dial
                    .map_err(EitherError::B)
                    .map_ok(EitherOutput::Second)
                    .boxed();
                Ok(future::select_ok([dial]).map_ok(|res| res.0).boxed())
            }
            (Ok(dial1), Ok(dial2)) => {
                let dial1 = dial1
                    .map_err(EitherError::A)
                    .map_ok(EitherOutput::First)
                    .boxed();
                let dial2 = dial2
                    .map_err(EitherError::B)
                    .map_ok(EitherOutput::Second)
                    .boxed();

                Ok(future::select_ok([dial1, dial2])
                    .map_ok(|res| res.0)
                    .boxed())
            }
        }
    }
}

impl<A, B> Transport for CombineTransport<A, B>
where
    B: Transport,
    A: Transport,
    A::Output: Send + 'static,
    B::Output: Send + 'static,
    A::Error: Send + 'static,
    B::Error: Send + 'static,
    A::Dial: Send + 'static,
    B::Dial: Send + 'static,
{
    type Output = EitherOutput<A::Output, B::Output>;
    type Error = EitherError<A::Error, B::Error>;
    type ListenerUpgrade = EitherFuture<A::ListenerUpgrade, B::ListenerUpgrade>;
    type Dial = BoxFuture<
        'static,
        Result<EitherOutput<A::Output, B::Output>, EitherError<A::Error, B::Error>>,
    >;

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
        self._dial(addr, false)
    }

    fn dial_as_listener(
        &mut self,
        addr: Multiaddr,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        self._dial(addr, true)
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
