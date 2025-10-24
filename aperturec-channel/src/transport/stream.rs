//! Byte-oriented, reliable, in-order transport
use super::{macros::*, *};
use crate::{
    quic,
    util::{Syncify, SyncifyLazy},
};

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

impl AsyncReceive for ReceiveStream {
    type Error = RxError;
    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        self.receive()
            .await
            .map_err(quic::Error::from)?
            .ok_or(RxError::Empty)
    }
}

impl AsyncReceive for BidirectionalStream {
    type Error = RxError;
    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        self.receive()
            .await
            .map_err(quic::Error::from)?
            .ok_or(RxError::Empty)
    }
}

impl AsyncTransmit for SendStream {
    type Error = TxError;
    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        Ok(self.send(data).await.map_err(quic::Error::from)?)
    }
}

impl AsyncTransmit for BidirectionalStream {
    type Error = TxError;
    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        Ok(self.send(data).await.map_err(quic::Error::from)?)
    }
}

impl AsyncFlush for SendStream {
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.flush().await.map_err(quic::Error::from)?)
    }
}

impl AsyncFlush for BidirectionalStream {
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.flush().await.map_err(quic::Error::from)?)
    }
}

impl AsyncSplitable for BidirectionalStream {
    type ReceiveHalf = ReceiveStream;
    type TransmitHalf = SendStream;

    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
        self.split()
    }
}

macro_rules! stream_type_sync {
    ($type:ident, $stream_type:ty, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: $stream_type,
            async_rt: Arc<TokioRuntime>,
        }

        impl AsRef<$stream_type> for $type {
            fn as_ref(&self) -> &$stream_type {
                &self.transport
            }
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(stream: $stream_type, async_rt: Arc<TokioRuntime>) -> Self {
                Self {
                    transport: stream,
                    async_rt,
                }
            }
        }
    };
}

macro_rules! stream_type_async {
    ($type:ident, $stream_type:ty, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: $stream_type,
        }

        impl AsRef<$stream_type> for $type {
            fn as_ref(&self) -> &$stream_type {
                &self.transport
            }
        }

        impl $type {
            /// Create a new [`Self`]
            pub fn new(stream: $stream_type) -> Self {
                Self { transport: stream }
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
delegate_transport_timeout_rx_sync!(Transceiver);
delegate_transport_tx_sync!(Transceiver, TxError);
delegate_transport_flush_sync!(Transceiver, TxError);
delegate_transport_split_sync!(Transceiver);

stream_type_sync!(Transmitter, SendStream, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter, TxError);
delegate_transport_flush_sync!(Transmitter, TxError);

stream_type_sync!(Receiver, ReceiveStream, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver, RxError);
delegate_transport_timeout_rx_sync!(Receiver);

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
pub(crate) mod tests {
    use super::*;
    use crate::transport::{AsyncReceive, AsyncTransmit, Receive, TimeoutReceive, Transmit};
    use std::time::Duration;
    use tokio::sync::mpsc;

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

    impl AsyncTransmit for mpsc::UnboundedSender<u8> {
        type Error = TxError;
        async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
            for byte in data {
                self.send(byte).expect("rx closed");
            }
            Ok(())
        }
    }

    impl AsyncReceive for mpsc::UnboundedReceiver<u8> {
        type Error = RxError;
        async fn receive(&mut self) -> Result<Bytes, Self::Error> {
            let Some(first) = self.recv().await else {
                return Err(RxError::Empty);
            };

            let mut bytes = vec![first];
            self.recv_many(&mut bytes, self.len()).await;
            Ok(bytes.into())
        }
    }

    stream_type_sync!(
        InMemTransmitter,
        mpsc::UnboundedSender<u8>,
        "sync in-memory transmitter"
    );
    delegate_transport_tx_sync!(InMemTransmitter, TxError);

    stream_type_sync!(
        InMemReceiver,
        mpsc::UnboundedReceiver<u8>,
        "sync in-memory receiver"
    );
    delegate_transport_rx_sync!(InMemReceiver, RxError);
    delegate_transport_timeout_rx_sync!(InMemReceiver);

    stream_type_async!(
        AsyncInMemTransmitter,
        mpsc::UnboundedSender<u8>,
        "async in-memory transmitter"
    );
    delegate_transport_tx_async!(AsyncInMemTransmitter, TxError);

    stream_type_async!(
        AsyncInMemReceiver,
        mpsc::UnboundedReceiver<u8>,
        "async in-memory receiver"
    );
    delegate_transport_rx_async!(AsyncInMemReceiver, RxError);

    pub(crate) fn in_mem_sync_tx_sync_rx() -> (InMemTransmitter, InMemReceiver) {
        let rt = Arc::new(TokioRuntime::new().expect("runtime"));
        let (tx, rx) = mpsc::unbounded_channel();
        (
            InMemTransmitter::new(tx, rt.clone()),
            InMemReceiver::new(rx, rt.clone()),
        )
    }

    pub(crate) fn in_mem_sync_tx_async_rx(
        rt: Arc<TokioRuntime>,
    ) -> (InMemTransmitter, AsyncInMemReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InMemTransmitter::new(tx, rt), AsyncInMemReceiver::new(rx))
    }

    pub(crate) fn in_mem_async_tx_sync_rx(
        rt: Arc<TokioRuntime>,
    ) -> (AsyncInMemTransmitter, InMemReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (AsyncInMemTransmitter::new(tx), InMemReceiver::new(rx, rt))
    }

    pub(crate) fn in_mem_async_tx_async_rx() -> (AsyncInMemTransmitter, AsyncInMemReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (AsyncInMemTransmitter::new(tx), AsyncInMemReceiver::new(rx))
    }

    #[test]
    fn sync_round_trip() {
        let (mut tx, mut rx) = in_mem_sync_tx_sync_rx();

        tx.transmit(Bytes::from_static(b"hello")).expect("tx hello");
        tx.transmit(Bytes::from_static(b"world")).expect("tx world");

        let received = rx.receive().expect("receive data");
        assert_eq!(received.as_ref(), b"helloworld");
    }

    #[test]
    fn sync_timeout() {
        let (mut tx, mut rx) = in_mem_sync_tx_sync_rx();
        assert_eq!(
            rx.receive_timeout(Duration::from_millis(10)).expect("rx 1"),
            None
        );
        tx.transmit(Bytes::from_static(b"asdf")).expect("tx 1");
        let received = rx
            .receive_timeout(Duration::from_millis(10))
            .expect("rx 2")
            .expect("no data");
        assert_eq!(received.as_ref(), b"asdf");
    }

    #[tokio::test]
    async fn async_round_trip() {
        let (mut tx, mut rx) = in_mem_async_tx_async_rx();

        tx.transmit(Bytes::from_static(b"hello"))
            .await
            .expect("tx hello");
        tx.transmit(Bytes::from_static(b"world"))
            .await
            .expect("tx world");

        let received = rx.receive().await.expect("receive data");
        assert_eq!(received.as_ref(), b"helloworld");
    }

    #[test]
    fn async_to_sync_round_trip() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_async_tx_sync_rx(runtime.clone());

        runtime.block_on(async move {
            tx.transmit(Bytes::from_static(b"hello"))
                .await
                .expect("tx hello");
            tx.transmit(Bytes::from_static(b"world"))
                .await
                .expect("tx world");
        });

        let received = rx.receive().expect("receive data");
        assert_eq!(received.as_ref(), b"helloworld");
    }

    #[test]
    fn async_to_sync_timeout() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_async_tx_sync_rx(runtime.clone());

        assert_eq!(
            rx.receive_timeout(Duration::from_millis(10)).expect("rx 1"),
            None
        );
        runtime.block_on(async move {
            tx.transmit(Bytes::from_static(b"asdf"))
                .await
                .expect("tx 1");
        });

        let received = rx
            .receive_timeout(Duration::from_millis(10))
            .expect("rx 2")
            .expect("no data");
        assert_eq!(received.as_ref(), b"asdf");
    }

    #[test]
    fn sync_to_async_round_trip() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_sync_tx_async_rx(runtime.clone());

        tx.transmit(Bytes::from_static(b"hello")).expect("tx hello");
        tx.transmit(Bytes::from_static(b"world")).expect("tx world");

        let received = runtime.block_on(async move { rx.receive().await.expect("receive data") });
        assert_eq!(received.as_ref(), b"helloworld");
    }
}
