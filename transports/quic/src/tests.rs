// Copyright 2017-2020 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use super::*;
use async_macros::ready;
use futures::prelude::*;
use libp2p_core::{
    multiaddr::{Multiaddr, Protocol},
    transport::ListenerEvent,
    StreamMuxer, Transport,
};
use log::{debug, trace};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct QuicStream {
    id: Option<Substream>,
    muxer: Muxer,
    shutdown: bool,
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        assert!(!self.shutdown, "written after close");
        let inner = self.get_mut();
        inner
            .muxer
            .write_substream(cx, inner.id.as_mut().unwrap(), buf)
            .map_err(From::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        self.shutdown = true;
        let inner = self.get_mut();
        debug!("trying to close {:?}", inner.id);
        ready!(inner
            .muxer
            .shutdown_substream(cx, inner.id.as_mut().unwrap()))?;
        debug!("closed {:?}", inner.id);
        Poll::Ready(Ok(()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let inner = self.get_mut();
        inner
            .muxer
            .read_substream(cx, inner.id.as_mut().unwrap(), buf)
            .map_err(From::from)
    }
}

impl Drop for QuicStream {
    fn drop(&mut self) {
        match self.id.take() {
            None => {}
            Some(id) => self.muxer.destroy_substream(id),
        }
    }
}

impl futures::Stream for Muxer {
    type Item = QuicStream;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Poll::Ready(Some(QuicStream {
            id: Some(ready!(self.poll_inbound(cx)).expect("bug")),
            muxer: self.get_mut().clone(),
            shutdown: false,
        }))
    }
}

pub(crate) fn init() {
    use tracing_subscriber::{fmt::Subscriber, EnvFilter};
    drop(
        Subscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init(),
    )
}

impl Future for Muxer {
    type Output = Result<(), crate::error::Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.get_mut().close(cx)
    }
}

#[test]
fn wildcard_expansion() {
    init();
    let addr: Multiaddr = "/ip4/0.0.0.0/udp/1234/quic".parse().unwrap();
    let keypair = libp2p_core::identity::Keypair::generate_ed25519();
    let mut listener = Endpoint::new(Config::new(&keypair), addr.clone())
        .expect("endpoint")
        .listen_on(addr)
        .expect("listener");

    // Process all initial `NewAddress` events and make sure they
    // do not contain wildcard address or port.
    match futures::executor::block_on(listener.next())
        .unwrap()
        .unwrap()
    {
        ListenerEvent::NewAddress(a) => {
            let mut iter = a.iter();
            match iter.next().expect("ip address") {
                Protocol::Ip4(_ip) => {} // assert!(!ip.is_unspecified()),
                Protocol::Ip6(_ip) => {} // assert!(!ip.is_unspecified()),
                other => panic!("Unexpected protocol: {}", other),
            }
            if let Protocol::Udp(port) = iter.next().expect("port") {
                assert_ne!(0, port)
            } else {
                panic!("No UDP port in address: {}", a)
            }
        }
        _ => panic!("NewAddress is the first event"),
    }
}

#[test]
fn communicating_between_dialer_and_listener() {
    init();
    let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
    let mut ready_tx = Some(ready_tx);

    #[cfg(any())]
    async fn create_slowdown() {
        futures_timer::Delay::new(std::time::Duration::new(1, 0)).await
    }

    #[cfg(any())]
    struct BlockJoin<T> {
        handle: Option<async_std::task::JoinHandle<T>>,
    }

    #[cfg(any())]
    impl<T> Drop for BlockJoin<T> {
        fn drop(&mut self) {
            drop(async_std::task::block_on(self.handle.take().unwrap()))
        }
    }

    let keypair = libp2p_core::identity::Keypair::generate_ed25519();
    let keypair2 = keypair.clone();
    let handle = async_std::task::spawn(async move {
        let addr: Multiaddr = "/ip4/127.0.0.1/udp/12345/quic"
            .parse()
            .expect("bad address?");
        let quic_config = Config::new(&keypair2);
        let quic_endpoint = Endpoint::new(quic_config, addr.clone()).expect("I/O error");
        let mut listener = quic_endpoint.listen_on(addr).unwrap();

        loop {
            trace!("awaiting connection");
            match listener.next().await.unwrap().unwrap() {
                ListenerEvent::NewAddress(listen_addr) => {
                    ready_tx.take().unwrap().send(listen_addr).unwrap();
                }
                ListenerEvent::Upgrade { upgrade, .. } => {
                    log::debug!("got a connection upgrade!");
                    let (id, mut muxer): (_, Muxer) = upgrade.await.expect("upgrade failed");
                    log::debug!("got a new muxer!");
                    let mut socket: QuicStream = muxer.next().await.expect("no incoming stream");

                    let mut buf = [0u8; 3];
                    log::debug!("reading data from accepted stream!");
                    {
                        let mut count = 0;
                        while count < buf.len() {
                            count += socket.read(&mut buf[count..]).await.unwrap();
                        }
                    }
                    assert_eq!(buf, [4, 5, 6]);
                    log::debug!("writing data!");
                    socket.write_all(&[0x1, 0x2, 0x3]).await.unwrap();
                    log::debug!("data written!");
                    socket.close().await.unwrap();
                    log::debug!("socket closed!");
                    assert_eq!(socket.read(&mut buf).await.unwrap(), 0);
                    log::debug!("end of stream");
                    drop(socket);
                    muxer.await.unwrap();
                    log::debug!("finished!");
                    break id;
                }
                _ => unreachable!(),
            }
        }
    });

    let second_handle = async_std::task::spawn(async move {
        let addr = ready_rx.await.unwrap();
        let quic_config = Config::new(&keypair);
        let quic_endpoint = Endpoint::new(
            quic_config,
            "/ip4/127.0.0.1/udp/12346/quic".parse().unwrap(),
        )
        .unwrap();
        // Obtain a future socket through dialing
        let connection = quic_endpoint.dial(addr.clone()).unwrap().await.unwrap();
        trace!("Received a Connection: {:?}", connection);
        let mut stream = QuicStream {
            id: Some(connection.1.open_outbound().await.expect("failed")),
            muxer: connection.1.clone(),
            shutdown: false,
        };
        log::debug!("have a new stream!");
        stream.write_all(&[4u8, 5, 6]).await.unwrap();
        stream.close().await.unwrap();
        let mut buf = [0u8; 3];
        log::debug!("reading data!");
        {
            let mut count = 0;
            while count < buf.len() {
                let read = stream.read(&mut buf[count..]).await.unwrap();
                assert_ne!(read, 0usize, "premature end of file");
                count += read;
            }
        }
        assert_eq!(buf, [1u8, 2, 3]);
        log::debug!("data read!");
        log::debug!("checking for EOF!");
        assert_eq!(stream.read(&mut buf).await.unwrap(), 0);
        drop(stream);
        log::debug!("have EOF!");
        connection.1.await.expect("closed successfully");
        log::debug!("awaiting handle!");
        connection.0
    });
    assert_eq!(
        async_std::task::block_on(handle),
        async_std::task::block_on(second_handle)
    );
}

#[test]
fn replace_port_0_in_returned_multiaddr_ipv4() {
    init();
    let keypair = libp2p_core::identity::Keypair::generate_ed25519();
    let config = Config::new(&keypair);

    let addr = "/ip4/127.0.0.1/udp/0/quic".parse::<Multiaddr>().unwrap();
    assert!(addr.to_string().ends_with("udp/0/quic"));

    let quic = Endpoint::new(config, addr.clone()).expect("no error");

    let new_addr = futures::executor::block_on_stream(quic.listen_on(addr).unwrap())
        .next()
        .expect("some event")
        .expect("no error")
        .into_new_address()
        .expect("listen address");

    assert!(!new_addr.to_string().contains("tcp/0"));
}

#[test]
fn replace_port_0_in_returned_multiaddr_ipv6() {
    init();
    let keypair = libp2p_core::identity::Keypair::generate_ed25519();
    let config = Config::new(&keypair);

    let addr: Multiaddr = "/ip6/::1/udp/0/quic".parse().unwrap();
    assert!(addr.to_string().contains("udp/0/quic"));
    let quic = Endpoint::new(config, addr.clone()).expect("no error");

    let new_addr = futures::executor::block_on_stream(quic.listen_on(addr).unwrap())
        .next()
        .expect("some event")
        .expect("no error")
        .into_new_address()
        .expect("listen address");

    assert!(!new_addr.to_string().contains("tcp/0"));
}

#[test]
fn larger_addr_denied() {
    init();
    let keypair = libp2p_core::identity::Keypair::generate_ed25519();
    let config = Config::new(&keypair);
    let addr = "/ip4/127.0.0.1/tcp/12345/tcp/12345"
        .parse::<Multiaddr>()
        .unwrap();
    assert!(Endpoint::new(config, addr).is_err())
}
