//! Byte-oriented, reliable, in-order transport
use super::{macros::*, *};
use crate::{quic, util::Syncify};

use bytes::Bytes;
use s2n_quic::stream::{BidirectionalStream, ReceiveStream, SendStream};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

/// Errors that occur at the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// A QUIC error occurred during transmission
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

/// Errors that occur at the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum RxError {
    /// A QUIC error occurred during reception
    #[error(transparent)]
    Quic(#[from] quic::Error),

    /// Attempted to read from a stream that is empty and closed.
    #[error("stream is empty and closed")]
    Empty,
}

#[derive(Debug)]
struct InOrderTransport<S> {
    stream: S,
}

impl AsyncReceive for InOrderTransport<ReceiveStream> {
    type Error = RxError;
    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        self.stream
            .receive()
            .await
            .map_err(quic::Error::from)?
            .ok_or(RxError::Empty)
    }
}

impl AsyncReceive for InOrderTransport<BidirectionalStream> {
    type Error = RxError;
    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        self.stream
            .receive()
            .await
            .map_err(quic::Error::from)?
            .ok_or(RxError::Empty)
    }
}

impl AsyncTransmit for InOrderTransport<SendStream> {
    type Error = TxError;
    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        Ok(self.stream.send(data).await.map_err(quic::Error::from)?)
    }
}

impl AsyncFlush for InOrderTransport<SendStream> {
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.stream.flush().await.map_err(quic::Error::from)?)
    }
}

impl AsyncTransmit for InOrderTransport<BidirectionalStream> {
    type Error = TxError;
    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        Ok(self.stream.send(data).await.map_err(quic::Error::from)?)
    }
}

impl AsyncFlush for InOrderTransport<BidirectionalStream> {
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.stream.flush().await.map_err(quic::Error::from)?)
    }
}

impl AsyncSplitable for InOrderTransport<BidirectionalStream> {
    type ReceiveHalf = InOrderTransport<ReceiveStream>;
    type TransmitHalf = InOrderTransport<SendStream>;

    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
        let (rh, th) = self.stream.split();
        (
            InOrderTransport { stream: rh },
            InOrderTransport { stream: th },
        )
    }
}

macro_rules! stream_type_sync {
    ($type:ident, $stream_type:ident, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: InOrderTransport<$stream_type>,
            async_rt: Arc<TokioRuntime>,
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(stream: $stream_type, async_rt: Arc<TokioRuntime>) -> Self {
                Self {
                    transport: InOrderTransport { stream },
                    async_rt,
                }
            }
        }
    };
}

macro_rules! stream_type_async {
    ($type:ident, $stream_type:ident, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: InOrderTransport<$stream_type>,
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(stream: $stream_type) -> Self {
                Self {
                    transport: InOrderTransport { stream },
                }
            }
        }
    };
}

stream_type_sync!(
    Transceiver,
    BidirectionalStream,
    "A byte-oriented sender and receiver"
);
delegate_transport_rx_sync!(Transceiver, RxError);
delegate_transport_tx_sync!(Transceiver, TxError);
delegate_transport_flush_sync!(Transceiver, TxError);
delegate_transport_split_sync!(Transceiver);

stream_type_sync!(Transmitter, SendStream, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter, TxError);
delegate_transport_flush_sync!(Transmitter, TxError);

stream_type_sync!(Receiver, ReceiveStream, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver, RxError);

stream_type_async!(
    AsyncTransceiver,
    BidirectionalStream,
    "Async variant of [`Transceiver`]"
);
delegate_transport_rx_async!(AsyncTransceiver, RxError);
delegate_transport_tx_async!(AsyncTransceiver, TxError);
delegate_transport_flush_async!(AsyncTransceiver, TxError);
delegate_transport_split_async!(AsyncTransceiver);

stream_type_async!(
    AsyncTransmitter,
    SendStream,
    "Async variant of [`Transmitter`]"
);
delegate_transport_tx_async!(AsyncTransmitter, TxError);
delegate_transport_flush_async!(AsyncTransmitter, TxError);

stream_type_async!(
    AsyncReceiver,
    ReceiveStream,
    "Async variant of [`Receiver`]"
);
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
    fn test_rx_error_empty_display() {
        let error = RxError::Empty;
        let display_str = format!("{}", error);
        assert_eq!(display_str, "stream is empty and closed");
    }

    #[test]
    fn test_rx_error_empty_debug() {
        let error = RxError::Empty;
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Empty"));
    }
}
