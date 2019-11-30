// Copyright 2017-2018 Parity Technologies (UK) Ltd.
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

//! Implementation of the libp2p `Transport` trait for QUIC/UDP/IP.
//!
//! Uses [the *tokio* library](https://tokio.rs).
//!
//! # Usage
//!
//! Example:
//!
//! ```
//! extern crate libp2p_tcp;
//! use libp2p_tcp::QuicConfig;
//!
//! # fn main() {
//! let tcp = QuicConfig::new();
//! # }
//! ```
//!
//! The `QuicConfig` structs implements the `Transport` trait of the `swarm` library. See the
//! documentation of `swarm` and of libp2p in general to learn how to use the `Transport` trait.
//!
//! Note that QUIC provides transport, security, and multiplexing in a single protocol.  Therefore,
//! QUIC connections do not need to be upgraded.  You will get a compile-time error if you try.
//! Instead, you must pass all needed configuration into the constructor.

use futures::{
    future::{self, Either},
    prelude::*,
    stream::{self, Chain, Once, Stream},
};
use ipnet::IpNet;
use libp2p_core::{
    multiaddr::{host_addresses, ip_to_multiaddr, Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    StreamMuxer, Transport,
};
use log::debug;
pub use quinn::{Endpoint, EndpointBuilder, EndpointError, ServerConfig};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    iter::{self, FromIterator},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
    time::{Duration, Instant},
    vec::IntoIter,
};
use tokio_io::{AsyncRead, AsyncWrite};

/// Represents the configuration for a QUIC/UDP/IP transport capability for libp2p.
///
/// The QUIC endpoints created by libp2p will need to be progressed by running the futures and streams
/// obtained by libp2p through the tokio reactor.
#[derive(Debug, Clone)]
pub struct QuicConfig {
    /// The underlying QUIC transport config.  Quinn provides functions for creating a suitable
    /// one.
    pub endpoint_builder: EndpointBuilder,
    /// The underlying QUIC transport endpoint.
    endpoint: Option<Endpoint>,
    /// The server configuration.  Quinn provides functions for making one.
    pub server_configuration: ServerConfig,
}

/// An error in the QUIC transport
#[derive(Debug, err_derive::Error)]
pub enum QuicError {
    /// An I/O error
    #[error(display = "Endpoint error: {}", _0)]
    EndpointError(#[source] quinn::EndpointError),
    #[error(display = "QUIC Protocol Error: {}", _0)]
    ConnectionError(#[source] quinn::ConnectionError),
    #[error(display = "QUIC outbound connection error: {}", _0)]
    ConnectError(#[source] quinn::ConnectError),
}

impl QuicConfig {
    /// Creates a new configuration object for TCP/IP.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self::new()
    }
}

pub struct QuicIncoming {
	/// This field uses structural pinning…
    incoming: quinn::Incoming,
	/// but this field does not.
    addr: Multiaddr,
}

type CompatConnecting = future::MapErr<quinn::Connecting, fn(quinn::ConnectionError) -> QuicError>;

impl futures_core::stream::Stream for QuicIncoming {
    type Item = Result<ListenerEvent<CompatConnecting>, QuicError>;
    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.incoming).poll_next(ctx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(upgrade)) => {
                let peer = upgrade.remote_address();
                Poll::Ready(Some(Ok(ListenerEvent::Upgrade {
                    remote_addr: ip_to_multiaddr(
                        peer.ip(),
                        &[Protocol::Udp(peer.port()), Protocol::Quic],
                    ),
                    upgrade: upgrade.map_err(QuicError::ConnectionError as _),
                    local_addr: self.addr.clone(),
                })))
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

struct QuicMuxer {
    bi_streams: quinn::IncomingBiStreams,
    connection: quinn::Connection,
    driver: quinn::ConnectionDriver,
}

pub struct SyncQuicMuxer(Mutex<QuicMuxer>);

pub struct QuicSubstream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

// FIXME: if quinn ever starts using `!Unpin` futures, this will require `unsafe` code.
// Will probably fall back to spawning the connection driver in this case 🙁
impl StreamMuxer for SyncQuicMuxer {
    type OutboundSubstream = quinn::OpenBi;
    type Substream = (quinn::SendStream, quinn::RecvStream);
    type Error = quinn::ConnectionError;
    fn poll_inbound(&self, cx: &mut Context) -> Poll<Result<Self::Substream, Self::Error>> {
		let mut this = self.0.lock().unwrap();
        match Pin::new(&mut this.driver).poll(cx) {
            Poll::Ready(Ok(())) => unreachable!(
                "ConnectionDriver will not resolve as long as `self.bi_streams` is alive"
            ),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => (),
        }
        match Pin::new(&mut this.bi_streams).poll_next(cx) {
			Poll::Ready(Some(stream)) => Poll::Ready(stream),
			Poll::Ready(None) => Poll::Ready(Err(quinn::ConnectionError::LocallyClosed)),
			Poll::Pending => Poll::Pending,
		}
    }
    fn open_outbound(&self) -> Self::OutboundSubstream {
        self.0.lock().unwrap().connection.open_bi()
    }
	fn destroy_substream(&self, _substream: Self::Substream) {
	}
	fn destroy_outbound(&self, _substream: Self::OutboundSubstream) {
	}
	fn is_remote_acknowledged(&self) -> bool {
		true
	}
	fn write_substream(&self, cx: &mut Context, substream: &mut Self::Substream, buf: &[u8]) -> Poll<Result<usize, Self::Error>> {
		Pin::new(substream.0).poll_write(buf)
	}
	fn poll_outbound(&self, cx: &mut Context, _substream: &mut Self::OutboundSubstream) -> Poll<Result<Self::Substream, Self::Error>> {
		unimplemented!()
	}
	fn read_substream(&self, cx: &mut Context, _substream: &mut Self::Substream, _buf: &mut [u8]) -> Poll<Result<usize, Self::Error>> {
		unimplemented!()
	}
    fn shutdown_substream(&self, cx: &mut Context, _substream: &mut Self::Substream) -> Poll<Result<(), Self::Error>> {
		unimplemented!()
	}
    fn flush_substream(&self, cx: &mut Context, _substream: &mut Self::Substream) -> Poll<Result<(), Self::Error>> {
		unimplemented!()
	}
    fn flush_all(&self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
		unimplemented!()
	}
    fn close(&self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
		unimplemented!()
	}
}

impl Transport for QuicConfig {
    type Output = quinn::NewConnection;
    type Error = QuicError;
    type Listener = QuicIncoming;
    type ListenerUpgrade = CompatConnecting;
    type Dial = CompatConnecting;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        let socket_addr = if let Ok(sa) = multiaddr_to_socketaddr(&addr) {
            sa
        } else {
            return Err(TransportError::MultiaddrNotSupported(addr));
        };

        let (driver, _endpoint, incoming) = self
            .endpoint_builder
            .bind(&socket_addr)
            .map_err(|e| TransportError::Other(QuicError::EndpointError(e)))?;
        tokio::spawn(driver.map_err(drop).compat());
        Ok(QuicIncoming { incoming, addr })
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        let socket_addr = if let Ok(socket_addr) = multiaddr_to_socketaddr(&addr) {
            if socket_addr.port() == 0 || socket_addr.ip().is_unspecified() {
                debug!("Instantly refusing dialing {}, as it is invalid", addr);
                return Err(TransportError::Other(QuicError::EndpointError(
                    EndpointError::Socket(io::ErrorKind::ConnectionRefused.into()),
                )));
            }
            socket_addr
        } else {
            return Err(TransportError::MultiaddrNotSupported(addr));
        };

        let (driver, endpoint, _incoming) =
            self.endpoint_builder
                .bind(&([0u8; 16], 0u16).into())
                .map_err(|e| TransportError::Other(QuicError::EndpointError(e)))?;

        Ok(endpoint
            .connect(&socket_addr, &socket_addr.to_string())
            .map_err(QuicError::ConnectError)?
            .map_err(QuicError::ConnectionError as _))
    }
}

// This type of logic should probably be moved into the multiaddr package
fn multiaddr_to_socketaddr(addr: &Multiaddr) -> Result<SocketAddr, ()> {
    let mut iter = addr.iter();
    let proto1 = iter.next().ok_or(())?;
    let proto2 = iter.next().ok_or(())?;
    let proto3 = iter.next().ok_or(())?;

    if iter.next().is_some() {
        return Err(());
    }

    match (proto1, proto2, proto3) {
        (Protocol::Ip4(ip), Protocol::Udp(port), Protocol::Quic) => {
            Ok(SocketAddr::new(ip.into(), port))
        }
        (Protocol::Ip6(ip), Protocol::Udp(port), Protocol::Quic) => {
            Ok(SocketAddr::new(ip.into(), port))
        }
        _ => Err(()),
    }
}

/// Listen address information.
#[derive(Debug)]
enum Addresses {
    /// A specific address is used to listen.
    One(Multiaddr),
    /// A set of addresses is used to listen.
    Many(Vec<(IpAddr, IpNet, Multiaddr)>),
}

#[cfg(test)]
mod tests {
    use super::{multiaddr_to_socketaddr, Listener, TcpConfig};
    use futures::{
        future::{self, Loop},
        prelude::*,
        stream,
    };
    use libp2p_core::{
        multiaddr::{Multiaddr, Protocol},
        transport::ListenerEvent,
        Transport,
    };
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::Duration,
    };
    use tokio::runtime::current_thread::{self, Runtime};
    use tokio_io;

    #[test]
    fn pause_on_error() {
        // We create a stream of values and errors and continue polling even after errors
        // have been encountered. We count the number of items (including errors) and assert
        // that no item has been missed.
        let rs = stream::iter_result(vec![Ok(1), Err(1), Ok(1), Err(1)]);
        let ls = Listener::new(rs, Duration::from_secs(1));
        let sum = future::loop_fn((0, ls), |(acc, ls)| {
            ls.into_future().then(move |item| match item {
                Ok((None, _)) => Ok::<_, std::convert::Infallible>(Loop::Break(acc)),
                Ok((Some(n), rest)) => Ok(Loop::Continue((acc + n, rest))),
                Err((n, rest)) => Ok(Loop::Continue((acc + n, rest))),
            })
        });
        assert_eq!(4, current_thread::block_on_all(sum).unwrap())
    }

    #[test]
    fn wildcard_expansion() {
        let mut listener = TcpConfig::new()
            .listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())
            .expect("listener");

        // Get the first address.
        let addr = listener
            .by_ref()
            .wait()
            .next()
            .expect("some event")
            .expect("no error")
            .into_new_address()
            .expect("listen address");

        // Process all initial `NewAddress` events and make sure they
        // do not contain wildcard address or port.
        let server = listener
            .take_while(|event| match event {
                ListenerEvent::NewAddress(a) => {
                    let mut iter = a.iter();
                    match iter.next().expect("ip address") {
                        Protocol::Ip4(ip) => assert!(!ip.is_unspecified()),
                        Protocol::Ip6(ip) => assert!(!ip.is_unspecified()),
                        other => panic!("Unexpected protocol: {}", other),
                    }
                    if let Protocol::Tcp(port) = iter.next().expect("port") {
                        assert_ne!(0, port)
                    } else {
                        panic!("No TCP port in address: {}", a)
                    }
                    Ok(true)
                }
                _ => Ok(false),
            })
            .for_each(|_| Ok(()));

        let client = TcpConfig::new().dial(addr).expect("dialer");
        tokio::run(
            server
                .join(client)
                .map(|_| ())
                .map_err(|e| panic!("error: {}", e)),
        )
    }

    #[test]
    fn multiaddr_to_tcp_conversion() {
        use std::net::Ipv6Addr;

        assert!(
            multiaddr_to_socketaddr(&"/ip4/127.0.0.1/udp/1234".parse::<Multiaddr>().unwrap())
                .is_err()
        );

        assert_eq!(
            multiaddr_to_socketaddr(&"/ip4/127.0.0.1/tcp/12345".parse::<Multiaddr>().unwrap()),
            Ok(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                12345,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/255.255.255.255/tcp/8080"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Ok(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                8080,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(&"/ip6/::1/tcp/12345".parse::<Multiaddr>().unwrap()),
            Ok(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                12345,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/tcp/8080"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Ok(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(
                    65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
                )),
                8080,
            ))
        );
    }

    #[test]
    fn communicating_between_dialer_and_listener() {
        use std::io::Write;

        std::thread::spawn(move || {
            let addr = "/ip4/127.0.0.1/tcp/12345".parse::<Multiaddr>().unwrap();
            let tcp = TcpConfig::new();
            let mut rt = Runtime::new().unwrap();
            let handle = rt.handle();
            let listener = tcp
                .listen_on(addr)
                .unwrap()
                .filter_map(ListenerEvent::into_upgrade)
                .for_each(|(sock, _)| {
                    sock.and_then(|sock| {
                        // Define what to do with the socket that just connected to us
                        // Which in this case is read 3 bytes
                        let handle_conn = tokio_io::io::read_exact(sock, [0; 3])
                            .map(|(_, buf)| assert_eq!(buf, [1, 2, 3]))
                            .map_err(|err| panic!("IO error {:?}", err));

                        // Spawn the future as a concurrent task
                        handle.spawn(handle_conn).unwrap();

                        Ok(())
                    })
                });

            rt.block_on(listener).unwrap();
            rt.run().unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let addr = "/ip4/127.0.0.1/tcp/12345".parse::<Multiaddr>().unwrap();
        let tcp = TcpConfig::new();
        // Obtain a future socket through dialing
        let socket = tcp.dial(addr.clone()).unwrap();
        // Define what to do with the socket once it's obtained
        let action = socket.then(|sock| -> Result<(), ()> {
            sock.unwrap().write(&[0x1, 0x2, 0x3]).unwrap();
            Ok(())
        });
        // Execute the future in our event loop
        let mut rt = Runtime::new().unwrap();
        let _ = rt.block_on(action).unwrap();
    }

    #[test]
    fn replace_port_0_in_returned_multiaddr_ipv4() {
        let tcp = TcpConfig::new();

        let addr = "/ip4/127.0.0.1/tcp/0".parse::<Multiaddr>().unwrap();
        assert!(addr.to_string().contains("tcp/0"));

        let new_addr = tcp
            .listen_on(addr)
            .unwrap()
            .wait()
            .next()
            .expect("some event")
            .expect("no error")
            .into_new_address()
            .expect("listen address");

        assert!(!new_addr.to_string().contains("tcp/0"));
    }

    #[test]
    fn replace_port_0_in_returned_multiaddr_ipv6() {
        let tcp = TcpConfig::new();

        let addr: Multiaddr = "/ip6/::1/tcp/0".parse().unwrap();
        assert!(addr.to_string().contains("tcp/0"));

        let new_addr = tcp
            .listen_on(addr)
            .unwrap()
            .wait()
            .next()
            .expect("some event")
            .expect("no error")
            .into_new_address()
            .expect("listen address");

        assert!(!new_addr.to_string().contains("tcp/0"));
    }

    #[test]
    fn larger_addr_denied() {
        let tcp = TcpConfig::new();

        let addr = "/ip4/127.0.0.1/tcp/12345/tcp/12345"
            .parse::<Multiaddr>()
            .unwrap();
        assert!(tcp.listen_on(addr).is_err());
    }
}
struct QuicTransport {
    endpoint: quinn::Endpoint,
}
