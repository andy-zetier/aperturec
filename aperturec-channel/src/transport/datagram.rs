//! Byte-oriented, reliable, out-of-order transport
use crate::quic;
use crate::transport::{AsyncReceive, AsyncTransmit, macros::*};
use crate::util::Syncify;

use bytes::Bytes;
use futures::prelude::{stream::FuturesUnordered, *};
use s2n_quic::connection::Connection;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

/// Errors that can occur transmitting datagrams
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// A QUIC error occurred during datagram transmission
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

/// Errors that can occur receiving datagrams
#[derive(Debug, thiserror::Error)]
pub enum RxError {
    /// Connection closed gracefully while waiting to accept a new stream.
    #[error("connection closed without error while accepting streams")]
    Accept,

    /// Failed to read data from a QUIC stream
    #[error("read datagram QUIC stream: {0}")]
    ReadStream(#[source] io::Error),

    /// A QUIC error occurred during datagram reception
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

type RxStreamFuture = Pin<Box<dyn Future<Output = Result<Bytes, RxError>> + Send + Sync>>;

#[derive(Debug)]
struct OutOfOrderTransport {
    connection: Connection,
    rx_streams: FuturesUnordered<RxStreamFuture>,
}

impl OutOfOrderTransport {
    fn new(connection: Connection) -> Self {
        OutOfOrderTransport {
            connection,
            rx_streams: FuturesUnordered::new(),
        }
    }
}

impl AsyncReceive for OutOfOrderTransport {
    type Error = RxError;

    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        loop {
            tokio::select! {
                new_stream_res = self.connection.accept_receive_stream() => {
                    match new_stream_res {
                        Ok(Some(mut stream)) => {
                            let mut data = vec![];
                            self.rx_streams.push(Box::pin(async move {
                                stream.read_to_end(&mut data).await.map_err(RxError::ReadStream)?;
                                Ok(data.into())
                            }));
                        }
                        Ok(None) => break Err(RxError::Accept),
                        Err(e) => break Err(quic::Error::from(e).into()),
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
    type Error = TxError;

    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        let mut stream = self
            .connection
            .open_send_stream()
            .await
            .map_err(quic::Error::from)?;
        stream.send(data).await.map_err(quic::Error::from)?;
        stream.finish().map_err(quic::Error::from)?;
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

        impl AsRef<Connection> for $type {
            fn as_ref(&self) -> &Connection {
                &self.transport.connection
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

        impl AsRef<Connection> for $type {
            fn as_ref(&self) -> &Connection {
                &self.transport.connection
            }
        }
    };
}

dg_type_sync!(Transceiver, "A byte-oriented sender and receiver");
delegate_transport_rx_sync!(Transceiver, RxError);
delegate_transport_tx_sync!(Transceiver, TxError);

dg_type_sync!(Transmitter, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter, TxError);

dg_type_sync!(Receiver, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver, RxError);

dg_type_async!(AsyncTransceiver, "Async variant of [`Transceiver`]");
delegate_transport_rx_async!(AsyncTransceiver, RxError);
delegate_transport_tx_async!(AsyncTransceiver, TxError);

dg_type_async!(AsyncTransmitter, "Async variant of [`Transmitter`]");
delegate_transport_tx_async!(AsyncTransmitter, TxError);

dg_type_async!(AsyncReceiver, "Async variant of [`Receiver`]");
delegate_transport_rx_async!(AsyncReceiver, RxError);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxError>();
    }

    #[test]
    fn test_rx_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RxError>();
    }

    #[test]
    fn test_tx_error_from_quic() {
        let quic_err = quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let error: TxError = quic_err.into();
        assert!(matches!(error, TxError::Quic(_)));
    }

    #[test]
    fn test_rx_error_from_quic() {
        let quic_err = quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let error: RxError = quic_err.into();
        assert!(matches!(error, RxError::Quic(_)));
    }

    #[test]
    fn test_rx_error_accept_display() {
        let error = RxError::Accept;
        let display_str = format!("{}", error);
        assert_eq!(
            display_str,
            "connection closed without error while accepting streams"
        );
    }

    #[test]
    fn test_rx_error_read_stream() {
        let io_err = io::Error::new(io::ErrorKind::UnexpectedEof, "test");
        let error = RxError::ReadStream(io_err);
        let display_str = format!("{}", error);
        assert!(display_str.contains("read datagram QUIC stream"));
    }

    #[test]
    fn test_rx_error_debug() {
        let error = RxError::Accept;
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Accept"));
    }
}
