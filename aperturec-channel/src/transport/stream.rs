//! Byte-oriented, reliable, in-order transport
use super::{AsyncFlush, AsyncReceive, AsyncSplitable, AsyncTransmit, macros::*};
use crate::util::Syncify;

use anyhow::{Result, bail};
use bytes::Bytes;
use s2n_quic::stream::{BidirectionalStream, ReceiveStream, SendStream};
use std::sync::Arc;
use tokio::runtime::Runtime as TokioRuntime;

#[derive(Debug)]
struct InOrderTransport<S> {
    stream: S,
}

impl AsyncReceive for InOrderTransport<ReceiveStream> {
    async fn receive(&mut self) -> Result<Bytes> {
        match self.stream.receive().await? {
            Some(bytes) => Ok(bytes),
            None => bail!("empty stream"),
        }
    }
}

impl AsyncReceive for InOrderTransport<BidirectionalStream> {
    async fn receive(&mut self) -> Result<Bytes> {
        match self.stream.receive().await? {
            Some(bytes) => Ok(bytes),
            None => bail!("empty stream"),
        }
    }
}

impl AsyncTransmit for InOrderTransport<SendStream> {
    async fn transmit(&mut self, data: Bytes) -> Result<()> {
        Ok(self.stream.send(data).await?)
    }
}

impl AsyncFlush for InOrderTransport<SendStream> {
    async fn flush(&mut self) -> Result<()> {
        Ok(self.stream.flush().await?)
    }
}

impl AsyncTransmit for InOrderTransport<BidirectionalStream> {
    async fn transmit(&mut self, data: Bytes) -> Result<()> {
        Ok(self.stream.send(data).await?)
    }
}

impl AsyncFlush for InOrderTransport<BidirectionalStream> {
    async fn flush(&mut self) -> Result<()> {
        Ok(self.stream.flush().await?)
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
delegate_transport_rx_sync!(Transceiver);
delegate_transport_tx_sync!(Transceiver);
delegate_transport_flush_sync!(Transceiver);
delegate_transport_split_sync!(Transceiver);

stream_type_sync!(Transmitter, SendStream, "A byte-oriented sender");
delegate_transport_tx_sync!(Transmitter);
delegate_transport_flush_sync!(Transmitter);

stream_type_sync!(Receiver, ReceiveStream, "A byte-oriented receiver");
delegate_transport_rx_sync!(Receiver);

stream_type_async!(
    AsyncTransceiver,
    BidirectionalStream,
    "Async variant of [`Transceiver`]"
);
delegate_transport_rx_async!(AsyncTransceiver);
delegate_transport_tx_async!(AsyncTransceiver);
delegate_transport_flush_async!(AsyncTransceiver);
delegate_transport_split_async!(AsyncTransceiver);

stream_type_async!(
    AsyncTransmitter,
    SendStream,
    "Async variant of [`Transmitter`]"
);
delegate_transport_tx_async!(AsyncTransmitter);
delegate_transport_flush_async!(AsyncTransmitter);

stream_type_async!(
    AsyncReceiver,
    ReceiveStream,
    "Async variant of [`Receiver`]"
);
delegate_transport_rx_async!(AsyncReceiver);
