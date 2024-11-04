//! Low-level, byte-oriented API
use anyhow::Result;
use bytes::Bytes;

pub mod datagram;
pub mod stream;

/// A trait for types which can receive bytes
pub trait Receive {
    /// Receive a datagram or return an error
    fn receive(&mut self) -> Result<Bytes>;
}

/// A trait for types which can send bytes
pub trait Transmit {
    /// Send some bytes or return an error
    fn transmit(&mut self, data: Bytes) -> Result<()>;

    /// Ensure all bytes which have been sent are received by the other side
    fn flush(&mut self) -> Result<()>;
}

/// A unifying trait for all types which are both [`Transmit`] and [`Receive`]
pub trait Duplex: Transmit + Receive {}
impl<T: Transmit + Receive> Duplex for T {}

/// A trait for stream transports which can be split into receive and transmit halves
pub trait Splitable {
    /// The receive half type
    type ReceiveHalf: Receive;
    /// The transmit half type
    type TransmitHalf: Transmit;

    /// Split the transport into it's two parts
    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf);
}

mod async_variants {
    use super::*;

    #[trait_variant::make(Receive: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Receive`]
    pub trait LocalReceive: Send + Sized {
        async fn receive(&mut self) -> Result<Bytes>;
    }

    #[trait_variant::make(Transmit: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Transmit`]
    pub trait LocalTransmit: Send + Sized {
        async fn transmit(&mut self, data: Bytes) -> Result<()>;
        async fn flush(&mut self) -> Result<()>;
    }

    #[trait_variant::make(Splitable: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Splitable`]
    pub trait LocalSplitable {
        /// The receive half type
        type ReceiveHalf: AsyncReceive;
        /// The transmit half type
        type TransmitHalf: AsyncTransmit;

        /// Split the transport into it's two parts
        fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf);
    }

    #[trait_variant::make(Duplex: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Duplex`]
    pub trait LocalDuplex: Transmit + Receive {}
    impl<T: Transmit + Receive> Duplex for T {}
}
pub use async_variants::Duplex as AsyncDuplex;
pub use async_variants::Receive as AsyncReceive;
pub use async_variants::Splitable as AsyncSplitable;
pub use async_variants::Transmit as AsyncTransmit;

mod macros {
    macro_rules! delegate_transport_rx_sync {
        ($type:ident) => {
            impl crate::transport::Receive for $type {
                fn receive(&mut self) -> Result<Bytes> {
                    AsyncReceive::receive(&mut self.transport).syncify(&self.async_rt)
                }
            }
        };
    }

    macro_rules! delegate_transport_rx_async {
        ($type:ident) => {
            impl crate::transport::AsyncReceive for $type {
                async fn receive(&mut self) -> Result<Bytes> {
                    AsyncReceive::receive(&mut self.transport).await
                }
            }
        };
    }

    macro_rules! delegate_transport_tx_sync {
        ($type:ident) => {
            impl crate::transport::Transmit for $type {
                fn transmit(&mut self, data: Bytes) -> Result<()> {
                    AsyncTransmit::transmit(&mut self.transport, data).syncify(&self.async_rt)
                }

                fn flush(&mut self) -> Result<()> {
                    AsyncTransmit::flush(&mut self.transport).syncify(&self.async_rt)
                }
            }
        };
    }

    macro_rules! delegate_transport_tx_async {
        ($type:ident) => {
            impl crate::transport::AsyncTransmit for $type {
                async fn transmit(&mut self, data: Bytes) -> Result<()> {
                    AsyncTransmit::transmit(&mut self.transport, data).await
                }

                async fn flush(&mut self) -> Result<()> {
                    AsyncTransmit::flush(&mut self.transport).await
                }
            }
        };
    }

    macro_rules! delegate_transport_split_sync {
        ($type:ident) => {
            impl crate::transport::Splitable for $type {
                type ReceiveHalf = Receiver;
                type TransmitHalf = Transmitter;

                fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
                    let (rh, th) = self.transport.split();
                    (
                        Receiver {
                            transport: rh,
                            async_rt: self.async_rt.clone(),
                        },
                        Transmitter {
                            transport: th,
                            async_rt: self.async_rt,
                        },
                    )
                }
            }
        };
    }

    macro_rules! delegate_transport_split_async {
        ($type:ident) => {
            impl crate::transport::AsyncSplitable for $type {
                type ReceiveHalf = AsyncReceiver;
                type TransmitHalf = AsyncTransmitter;

                fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
                    let (rh, th) = self.transport.split();
                    (
                        AsyncReceiver { transport: rh },
                        AsyncTransmitter { transport: th },
                    )
                }
            }
        };
    }

    pub(crate) use {
        delegate_transport_rx_async, delegate_transport_rx_sync, delegate_transport_split_async,
        delegate_transport_split_sync, delegate_transport_tx_async, delegate_transport_tx_sync,
    };
}
