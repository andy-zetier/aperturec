//! Byte-oriented, reliable, out-of-order transport
use crate::quic;
use crate::transport::{AsyncReceive, AsyncTransmit, macros::*, stream};
use crate::util::{Syncify, SyncifyLazy};

use bytes::{Bytes, BytesMut};
use futures::prelude::{stream::FuturesUnordered, *};
use s2n_quic::{
    connection::{Connection, Handle, StreamAcceptor},
    stream::{ReceiveStream, SendStream},
};
use std::error::Error;
use std::pin::Pin;
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

/// Errors that can occur transmitting datagrams
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// A QUIC error occurred during datagram transmission
    #[error(transparent)]
    Quic(#[from] quic::Error),

    /// An error with the underlying stream occured during transmission
    #[error(transparent)]
    Stream(#[from] super::stream::TxError),
}

/// Errors that can occur receiving datagrams
#[derive(Debug, thiserror::Error)]
pub enum RxError {
    /// Connection closed gracefully while waiting to accept a new stream.
    #[error("connection closed without error while accepting streams")]
    Accept,

    /// An error with the underlying stream occured during transmission
    #[error(transparent)]
    Stream(#[from] stream::RxError),

    /// A QUIC error occurred during datagram reception
    #[error(transparent)]
    Quic(#[from] quic::Error),
}

type RxStreamFuture = Pin<Box<dyn Future<Output = Result<Bytes, RxError>> + Send + Sync>>;

#[trait_variant::make(Send + Sync)]
trait Acceptor: Send {
    type Stream: AsyncReceive;
    type Error: Error;
    async fn accept(&mut self) -> Result<Self::Stream, Self::Error>;
}

#[trait_variant::make(Send + Sync)]
trait Opener {
    type Stream: AsyncTransmit;
    type Error: Error;
    async fn open(&mut self) -> Result<Self::Stream, Self::Error>;
}

impl Acceptor for StreamAcceptor {
    type Stream = ReceiveStream;
    type Error = RxError;

    async fn accept(&mut self) -> Result<Self::Stream, Self::Error> {
        self.accept_receive_stream()
            .await
            .map_err(quic::Error::from)?
            .ok_or(RxError::Accept)
    }
}

impl Opener for Handle {
    type Stream = SendStream;
    type Error = TxError;

    async fn open(&mut self) -> Result<Self::Stream, Self::Error> {
        Ok(self.open_send_stream().await.map_err(quic::Error::from)?)
    }
}

#[derive(Debug)]
struct OutOfOrderTransport<A, O> {
    acceptor: A,
    opener: O,
    rx_streams: FuturesUnordered<RxStreamFuture>,
}

impl<A, O> OutOfOrderTransport<A, O> {
    fn from_acceptor_opener(acceptor: A, opener: O) -> Self {
        OutOfOrderTransport {
            acceptor,
            opener,
            rx_streams: FuturesUnordered::new(),
        }
    }
}

impl<A, O> AsyncReceive for OutOfOrderTransport<A, O>
where
    A: Acceptor<Error = RxError> + Send + Sync,
    O: Send + Sync,
    <A as Acceptor>::Stream: AsyncReceive<Error = stream::RxError> + Send + Sync + Unpin + 'static,
{
    type Error = RxError;

    async fn receive(&mut self) -> Result<Bytes, Self::Error> {
        loop {
            tokio::select! {
                new_stream_res = self.acceptor.accept() => {
                    match new_stream_res {
                        Ok(mut stream) => {
                            self.rx_streams.push(Box::pin(async move {
                                let mut data = BytesMut::new();
                                loop {
                                    match stream.receive().await {
                                        Ok(byte_chunk) => data.extend(byte_chunk),
                                        Err(stream::RxError::Empty) => break Ok(data.freeze()),
                                        Err(error) => break Err(RxError::Stream(error)),
                                    }
                                }
                            }));
                        }
                        Err(e) => break Err(e),
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

impl<A, O: Opener> AsyncTransmit for OutOfOrderTransport<A, O>
where
    A: Send + Sync,
    TxError: From<<O as Opener>::Error>,
    TxError: From<<<O as Opener>::Stream as AsyncTransmit>::Error>,
{
    type Error = TxError;

    async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error> {
        let mut stream = self.opener.open().await?;
        stream.transmit(data).await?;
        Ok(())
    }
}

macro_rules! dg_type_sync {
    ($type:ident, $doc_com:expr) => {
        dg_type_sync!($type, StreamAcceptor, Handle, $doc_com);
        impl $type {
            /// Create a new [`Self`]
            pub fn new(connection: Connection, async_rt: Arc<TokioRuntime>) -> Self {
                let (opener, acceptor) = connection.split();
                Self::from_acceptor_opener(acceptor, opener, async_rt)
            }

            /// Get a handle to the underlying QUIC connection
            pub fn connection_handle(&self) -> Handle {
                self.transport.opener.clone()
            }
        }
    };
    ($type:ident, $acceptor:ty, $opener:ty, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: OutOfOrderTransport<$acceptor, $opener>,
            async_rt: Arc<TokioRuntime>,
        }

        impl $type {
            /// Create a new [`Self`] from an acceptor and opener
            fn from_acceptor_opener(
                acceptor: $acceptor,
                opener: $opener,
                async_rt: Arc<TokioRuntime>,
            ) -> Self {
                Self {
                    transport: OutOfOrderTransport::from_acceptor_opener(acceptor, opener),
                    async_rt,
                }
            }
        }
    };
}

macro_rules! dg_type_async {
    ($type:ident, $doc_com:expr) => {
        dg_type_async!($type, StreamAcceptor, Handle, $doc_com);
        impl $type {
            /// Create a new [`Self`]
            pub fn new(connection: Connection) -> Self {
                let (opener, acceptor) = connection.split();
                Self::from_acceptor_opener(acceptor, opener)
            }
        }
    };
    ($type:ident, $acceptor:ty, $opener:ty, $doc_com:expr) => {
        #[derive(Debug)]
        #[doc = $doc_com]
        pub struct $type {
            transport: OutOfOrderTransport<$acceptor, $opener>,
        }

        impl $type {
            /// Create a new [`Self`] from an acceptor and opener
            fn from_acceptor_opener(acceptor: $acceptor, opener: $opener) -> Self {
                Self {
                    transport: OutOfOrderTransport::from_acceptor_opener(acceptor, opener),
                }
            }
        }
    };
}

dg_type_sync!(Transceiver, "A byte-oriented sender and receiver");
delegate_transport_rx_sync!(Transceiver, RxError);
delegate_transport_timeout_rx_sync!(Transceiver);
delegate_transport_tx_sync!(Transceiver, TxError);

dg_type_sync!(Transmitter, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter, TxError);

dg_type_sync!(Receiver, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver, RxError);
delegate_transport_timeout_rx_sync!(Receiver);

dg_type_async!(AsyncTransceiver, "Async variant of [`Transceiver`]");
delegate_transport_rx_async!(AsyncTransceiver, RxError);
delegate_transport_tx_async!(AsyncTransceiver, TxError);

dg_type_async!(AsyncTransmitter, "Async variant of [`Transmitter`]");
delegate_transport_tx_async!(AsyncTransmitter, TxError);

dg_type_async!(AsyncReceiver, "Async variant of [`Receiver`]");
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
    fn test_rx_error_accept_display() {
        let error = RxError::Accept;
        let display_str = format!("{}", error);
        assert_eq!(
            display_str,
            "connection closed without error while accepting streams"
        );
    }

    #[test]
    fn test_rx_error_debug() {
        let error = RxError::Accept;
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Accept"));
    }

    impl Acceptor for mpsc::UnboundedReceiver<mpsc::UnboundedReceiver<u8>> {
        type Stream = mpsc::UnboundedReceiver<u8>;
        type Error = RxError;

        async fn accept(&mut self) -> Result<Self::Stream, Self::Error> {
            Ok(self.recv().await.expect("opener closed"))
        }
    }

    impl Opener for mpsc::UnboundedSender<mpsc::UnboundedReceiver<u8>> {
        type Stream = mpsc::UnboundedSender<u8>;
        type Error = TxError;

        async fn open(&mut self) -> Result<Self::Stream, Self::Error> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.send(rx).expect("acceptor closed");
            Ok(tx)
        }
    }

    dg_type_sync!(
        InMemTransmitter,
        (),
        mpsc::UnboundedSender::<mpsc::UnboundedReceiver::<u8>>,
        "sync in-memory transmitter"
    );
    delegate_transport_tx_sync!(InMemTransmitter, TxError);

    dg_type_sync!(
        InMemReceiver,
        mpsc::UnboundedReceiver::<mpsc::UnboundedReceiver::<u8>>,
        (),
        "sync in-memory receiver"
    );
    delegate_transport_rx_sync!(InMemReceiver, RxError);
    delegate_transport_timeout_rx_sync!(InMemReceiver);

    dg_type_async!(
        AsyncInMemTransmitter,
        (),
        mpsc::UnboundedSender::<mpsc::UnboundedReceiver::<u8>>,
        "async in-memory transmitter"
    );
    delegate_transport_tx_async!(AsyncInMemTransmitter, TxError);

    dg_type_async!(
        AsyncInMemReceiver,
        mpsc::UnboundedReceiver::<mpsc::UnboundedReceiver::<u8>>,
        (),
        "async in-memory receiver"
    );
    delegate_transport_rx_async!(AsyncInMemReceiver, RxError);

    pub(crate) fn in_mem_sync_tx_sync_rx() -> (InMemTransmitter, InMemReceiver) {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (tx_to_rx_sender, tx_to_rx_receiver) = mpsc::unbounded_channel();

        let transmitter =
            InMemTransmitter::from_acceptor_opener((), tx_to_rx_sender, runtime.clone());
        let receiver = InMemReceiver::from_acceptor_opener(tx_to_rx_receiver, (), runtime);
        (transmitter, receiver)
    }

    pub(crate) fn in_mem_sync_tx_async_rx(
        runtime: Arc<TokioRuntime>,
    ) -> (InMemTransmitter, AsyncInMemReceiver) {
        let (tx_to_rx_sender, tx_to_rx_receiver) = mpsc::unbounded_channel();

        let transmitter = InMemTransmitter::from_acceptor_opener((), tx_to_rx_sender, runtime);
        let receiver = AsyncInMemReceiver::from_acceptor_opener(tx_to_rx_receiver, ());
        (transmitter, receiver)
    }

    pub(crate) fn in_mem_async_tx_sync_rx(
        runtime: Arc<TokioRuntime>,
    ) -> (AsyncInMemTransmitter, InMemReceiver) {
        let (tx_to_rx_sender, tx_to_rx_receiver) = mpsc::unbounded_channel();

        let transmitter = AsyncInMemTransmitter::from_acceptor_opener((), tx_to_rx_sender);
        let receiver = InMemReceiver::from_acceptor_opener(tx_to_rx_receiver, (), runtime);
        (transmitter, receiver)
    }

    pub(crate) fn in_mem_async_tx_async_rx() -> (AsyncInMemTransmitter, AsyncInMemReceiver) {
        let (tx_to_rx_sender, tx_to_rx_receiver) = mpsc::unbounded_channel();

        let transmitter = AsyncInMemTransmitter::from_acceptor_opener((), tx_to_rx_sender);
        let receiver = AsyncInMemReceiver::from_acceptor_opener(tx_to_rx_receiver, ());
        (transmitter, receiver)
    }

    #[test]
    fn sync_round_trip() {
        let (mut tx, mut rx) = in_mem_sync_tx_sync_rx();

        tx.transmit(Bytes::from_static(b"hello")).expect("tx hello");
        tx.transmit(Bytes::from_static(b"world")).expect("tx world");

        let first = rx.receive().expect("rx 1");
        let second = rx.receive().expect("rx 2");
        assert_eq!(first.as_ref(), b"hello");
        assert_eq!(second.as_ref(), b"world");
    }

    #[test]
    fn sync_timeout() {
        let (mut tx, mut rx) = in_mem_sync_tx_sync_rx();

        assert_eq!(
            rx.receive_timeout(Duration::from_millis(10))
                .expect("timeout 1"),
            None
        );

        tx.transmit(Bytes::from_static(b"ping")).expect("tx ping");

        let received = rx
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout 2")
            .expect("datagram");
        assert_eq!(received.as_ref(), b"ping");
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

        let first = rx.receive().await.expect("rx 1");
        let second = rx.receive().await.expect("rx 2");
        assert_eq!(first.as_ref(), b"hello");
        assert_eq!(second.as_ref(), b"world");
    }

    #[test]
    fn async_to_sync_round_trip() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_async_tx_sync_rx(runtime.clone());

        runtime.block_on(async {
            tx.transmit(Bytes::from_static(b"hello"))
                .await
                .expect("tx hello");
            tx.transmit(Bytes::from_static(b"world"))
                .await
                .expect("tx world");
        });

        let first = rx.receive().expect("rx 1");
        let second = rx.receive().expect("rx 2");
        assert_eq!(first.as_ref(), b"hello");
        assert_eq!(second.as_ref(), b"world");
    }

    #[test]
    fn async_to_sync_timeout() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_async_tx_sync_rx(runtime.clone());

        assert_eq!(
            rx.receive_timeout(Duration::from_millis(10))
                .expect("timeout 1"),
            None
        );

        runtime.block_on(async {
            tx.transmit(Bytes::from_static(b"pong"))
                .await
                .expect("tx pong");
        });

        let received = rx
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout 2")
            .expect("datagram");
        assert_eq!(received.as_ref(), b"pong");
    }

    #[test]
    fn sync_to_async_round_trip() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (mut tx, mut rx) = in_mem_sync_tx_async_rx(runtime.clone());

        tx.transmit(Bytes::from_static(b"hello")).expect("tx hello");
        tx.transmit(Bytes::from_static(b"world")).expect("tx world");

        let (first, second) = runtime.block_on(async {
            let first = rx.receive().await.expect("rx 1");
            let second = rx.receive().await.expect("rx 2");
            (first, second)
        });
        assert_eq!(first.as_ref(), b"hello");
        assert_eq!(second.as_ref(), b"world");
    }
}
