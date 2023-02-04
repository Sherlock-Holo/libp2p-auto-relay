use std::io;
use std::io::{IoSlice, IoSliceMut};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures_util::{AsyncRead, AsyncWrite};
use libp2p_core::Multiaddr;
use libp2p_swarm::NegotiatedSubstream;

#[derive(Debug)]
pub struct Connection {
    remote_addr: Multiaddr,
    remaining_buf: Bytes,
    sub_stream: NegotiatedSubstream,
}

impl Connection {
    pub fn new(
        remote_addr: Multiaddr,
        remaining_buf: Bytes,
        sub_stream: NegotiatedSubstream,
    ) -> Self {
        Self {
            remote_addr,
            remaining_buf,
            sub_stream,
        }
    }

    pub fn remote_addr(&self) -> &Multiaddr {
        &self.remote_addr
    }
}

impl AsyncRead for Connection {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.remaining_buf.has_remaining() {
            let copied = self.remaining_buf.remaining().min(buf.len());

            buf[..copied].copy_from_slice(&self.remaining_buf[..copied]);
            self.remaining_buf.advance(copied);

            return Poll::Ready(Ok(copied));
        }

        Pin::new(&mut self.sub_stream).poll_read(cx, buf)
    }

    #[inline]
    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.remaining_buf.has_remaining() {
            let mut total = 0;
            for buf in bufs {
                let copied = self.remaining_buf.remaining().min(buf.len());
                if copied == 0 {
                    break;
                }

                buf.copy_from_slice(&self.remaining_buf[..copied]);
                self.remaining_buf.advance(copied);
                total += copied;
            }

            return Poll::Ready(Ok(total));
        }

        Pin::new(&mut self.sub_stream).poll_read_vectored(cx, bufs)
    }
}

impl AsyncWrite for Connection {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.sub_stream).poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.sub_stream).poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.sub_stream).poll_flush(cx)
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.sub_stream).poll_close(cx)
    }
}
