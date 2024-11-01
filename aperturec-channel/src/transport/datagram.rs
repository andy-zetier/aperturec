//! Byte-oriented, reliable, out-of-order transport
use crate::transport::{macros::*, AsyncReceive, AsyncTransmit};
use crate::util::Syncify;

use anyhow::{Error, Result};
use bytes::Bytes;
use futures::{
    future::BoxFuture,
    prelude::{stream::FuturesUnordered, *},
};
use s2n_quic::{connection::Connection, stream::SendStream};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[derive(Debug)]
struct OutOfOrderTransport {
    connection: Connection,
    rx_streams: FuturesUnordered<BoxFuture<'static, Result<Bytes>>>,
    tx_streams: Vec<SendStream>,
}

impl OutOfOrderTransport {
    fn new(connection: Connection) -> Self {
        OutOfOrderTransport {
            connection,
            rx_streams: FuturesUnordered::new(),
            tx_streams: Vec::new(),
        }
    }
}

impl AsyncReceive for OutOfOrderTransport {
    async fn receive(&mut self) -> Result<Bytes> {
        loop {
            tokio::select! {
                new_stream_res = self.connection.accept_receive_stream() => {
                    match new_stream_res {
                        Ok(Some(mut stream)) => {
                            let mut data = vec![];
                            self.rx_streams.push(async move {
                                stream.read_to_end(&mut data).await?;
                                Ok(data.into())
                            }.boxed());
                        }
                        Ok(None) => break Err(Error::msg("closed connection")),
                        Err(e) => break Err(e.into()),
                    }
                }
                Some(complete_stream) = self.rx_streams.next(), if !self.rx_streams.is_empty() => {
                    break complete_stream;
                }
                else => unreachable!("no complete streams"),
            }
        }
    }
}

impl AsyncTransmit for OutOfOrderTransport {
    async fn transmit(&mut self, data: Bytes) -> Result<()> {
        let mut stream = self.connection.open_send_stream().await?;
        stream.send(data).await?;
        stream.finish()?;
        self.tx_streams.push(stream);
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        while let Some(mut stream) = self.tx_streams.pop() {
            stream.flush().await?;
        }
        Ok(())
    }
}

macro_rules! dg_type_sync {
    ($type:ident, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: OutOfOrderTransport,
            async_rt: Arc<TokioRuntime>,
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(conn: Connection, async_rt: Arc<TokioRuntime>) -> Self {
                Self {
                    transport: OutOfOrderTransport::new(conn),
                    async_rt,
                }
            }

            /// Convert [`Self`] into the underlying QUIC connection
            pub fn into_connection(self) -> Connection {
                self.transport.connection
            }
        }
    };
}

macro_rules! dg_type_async {
    ($type:ident, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: OutOfOrderTransport,
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(conn: Connection) -> Self {
                Self {
                    transport: OutOfOrderTransport::new(conn),
                }
            }

            /// Convert [`Self`] into the underlying QUIC connection
            pub fn into_connection(self) -> Connection {
                self.transport.connection
            }
        }
    };
}

dg_type_sync!(Transceiver, "A byte-oriented sender and receiver");
delegate_transport_rx_sync!(Transceiver);
delegate_transport_tx_sync!(Transceiver);

dg_type_sync!(Transmitter, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter);

dg_type_sync!(Receiver, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver);

dg_type_async!(AsyncTransceiver, "Async variant of [`Transceiver`]");
delegate_transport_rx_async!(AsyncTransceiver);
delegate_transport_tx_async!(AsyncTransceiver);

dg_type_async!(AsyncTransmitter, "Async variant of [`Transmitter`]");
delegate_transport_tx_async!(AsyncTransmitter);

dg_type_async!(AsyncReceiver, "Async variant of [`Receiver`]");
delegate_transport_rx_async!(AsyncReceiver);
