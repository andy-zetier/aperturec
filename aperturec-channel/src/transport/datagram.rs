//! Byte-oriented, reliable, out-of-order transport
use crate::util::Syncify;

use anyhow::{Error, Result};
use futures::{
    future::BoxFuture,
    prelude::{stream::FuturesUnordered, *},
};
use s2n_quic::connection::Connection;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

use bytes::Bytes;

/// Maximum size to of data to pack into a datagram
pub const MAX_SIZE: usize = 1000;

/// A trait for types which can receive bytes
///
/// Analogous to [`std::io::Read`] but for datagrams instead of streams
pub trait Receive {
    /// Receive a datagram or return an error
    fn receive(&mut self) -> anyhow::Result<Bytes>;
}

/// A trait for types which can send bytes
///
/// Analogous to [`std::io::Write`] but for datagrams instead of streams
pub trait Transmit: Sized {
    /// Send a datagram or return an error
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()>;
}

mod async_variants {
    use bytes::Bytes;

    /// Async variant of [`Receive`](super::Receive)
    #[trait_variant::make(Receive: Send)]
    #[allow(dead_code)]
    pub trait LocalReceive {
        async fn receive(&mut self) -> anyhow::Result<Bytes>;
    }

    /// Async variant of [`Transmit`](super::Transmit)
    #[trait_variant::make(Transmit: Send)]
    #[allow(dead_code)]
    pub trait LocalTransmit: Sized {
        async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()>;
    }
}
pub use async_variants::{Receive as AsyncReceive, Transmit as AsyncTransmit};

#[derive(Debug)]
struct OutOfOrderConnection {
    inner: Connection,
    streams: FuturesUnordered<BoxFuture<'static, Result<Bytes>>>,
}

impl OutOfOrderConnection {
    fn new(inner: Connection) -> Self {
        OutOfOrderConnection {
            inner,
            streams: FuturesUnordered::new(),
        }
    }
}

impl AsyncReceive for OutOfOrderConnection {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        loop {
            tokio::select! {
                new_stream_res = self.inner.accept_receive_stream() => {
                    match new_stream_res {
                        Ok(Some(mut stream)) => {
                            let mut data = vec![];
                            self.streams.push(async move {
                                stream.read_to_end(&mut data).await?;
                                Ok(data.into())
                            }.boxed());
                        }
                        Ok(None) => break Err(Error::msg("closed connection")),
                        Err(e) => break Err(e.into()),
                    }
                }
                Some(complete_stream) = self.streams.next(), if !self.streams.is_empty() => {
                    break complete_stream;
                }
                else => unreachable!("no complete streams"),
            }
        }
    }
}

impl AsyncTransmit for Connection {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        Ok(self.open_send_stream().await?.send(data).await?)
    }
}

/// A byte-oriented sender and receiver
#[derive(Debug)]
pub struct Transceiver {
    conn: OutOfOrderConnection,
    async_rt: Arc<TokioRuntime>,
}

impl Transceiver {
    /// Create a new [`Self`]
    pub fn new(conn: Connection, async_rt: Arc<TokioRuntime>) -> Self {
        Transceiver {
            conn: OutOfOrderConnection::new(conn),
            async_rt,
        }
    }
}

impl Receive for Transceiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.conn).syncify(&self.async_rt)
    }
}

impl Transmit for Transceiver {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.conn.inner, data).syncify(&self.async_rt)
    }
}

/// A byte-oriented sender
#[derive(Debug)]
pub struct Transmitter {
    conn: Connection,
    async_rt: Arc<TokioRuntime>,
}

impl Transmitter {
    /// Create a new [`Self`]
    pub fn new(conn: Connection, async_rt: Arc<TokioRuntime>) -> Self {
        Transmitter { conn, async_rt }
    }
}

impl Transmit for Transmitter {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.conn, data).syncify(&self.async_rt)
    }
}

/// A byte-oriented receiver
#[derive(Debug)]
pub struct Receiver {
    conn: OutOfOrderConnection,
    async_rt: Arc<TokioRuntime>,
}

impl Receiver {
    /// Create a new [`Self`]
    pub fn new(conn: Connection, async_rt: Arc<TokioRuntime>) -> Self {
        Receiver {
            conn: OutOfOrderConnection::new(conn),
            async_rt,
        }
    }
}

impl Receive for Receiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.conn).syncify(&self.async_rt)
    }
}

/// Async variant of [`Transceiver`]
#[derive(Debug)]
pub struct AsyncTransceiver {
    conn: OutOfOrderConnection,
}

impl AsyncTransceiver {
    /// Create a new [`Self`]
    pub fn new(conn: Connection) -> Self {
        AsyncTransceiver {
            conn: OutOfOrderConnection::new(conn),
        }
    }
}

impl AsyncReceive for AsyncTransceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.conn).await
    }
}

impl AsyncTransmit for AsyncTransceiver {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.conn.inner, data).await
    }
}

/// Async variant of [`Transmitter`]
#[derive(Debug)]
pub struct AsyncTransmitter {
    conn: Connection,
}

impl AsyncTransmitter {
    /// Create a new [`Self`]
    pub fn new(conn: Connection) -> Self {
        AsyncTransmitter { conn }
    }
}

impl AsyncTransmit for AsyncTransmitter {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.conn, data).await
    }
}

/// Async variant of [`Receiver`]
#[derive(Debug)]
pub struct AsyncReceiver {
    conn: OutOfOrderConnection,
}

impl AsyncReceiver {
    /// Create a new [`Self`]
    pub fn new(conn: Connection) -> Self {
        AsyncReceiver {
            conn: OutOfOrderConnection::new(conn),
        }
    }
}

impl AsyncReceive for AsyncReceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.conn).await
    }
}
