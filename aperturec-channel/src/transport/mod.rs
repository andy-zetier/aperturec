//! Low-level, byte-oriented API
use bytes::Bytes;
use std::{error::Error, time::Duration};

pub mod datagram;
pub mod stream;

/// A trait for types which can receive bytes
pub trait Receive {
    /// The error type returned when receiving fails
    type Error: Error;
    /// Receive data or return an error
    fn receive(&mut self) -> Result<Bytes, Self::Error>;
}

/// A [`Receive`] implementation that can stop waiting once a timeout elapses.
///
/// # Timeout Semantics
///
/// Returns `Ok(Some(bytes))` when data arrives within the timeout, `Ok(None)` when the timeout
/// expires without data, and `Err(e)` for transport errors. Timeouts represent best-effort
/// maximum wait times
pub trait TimeoutReceive: Receive {
    /// Wait for up to `timeout` for data, returning `Ok(None)` when no bytes arrive in time.
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<Bytes>, Self::Error>;
}

/// A trait for types which can send bytes
pub trait Transmit {
    /// The error type returned when transmitting fails
    type Error: Error;

    /// Send some bytes or return an error
    fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error>;
}

/// A trait for types which can ensure all bytes have been received by the other side
pub trait Flush: Transmit {
    /// Block and ensure all bytes which have been sent are received by the other side
    fn flush(&mut self) -> Result<(), Self::Error>;
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

    #[trait_variant::make(Receive: Send + Sync)]
    #[allow(dead_code)]
    /// Async variant of [`super::Receive`]
    pub trait LocalReceive: Send + Sized {
        type Error: Error;
        async fn receive(&mut self) -> Result<Bytes, Self::Error>;
    }

    #[trait_variant::make(Transmit: Send + Sync)]
    #[allow(dead_code)]
    /// Async variant of [`super::Transmit`]
    pub trait LocalTransmit: Send + Sized {
        type Error: Error;
        async fn transmit(&mut self, data: Bytes) -> Result<(), Self::Error>;
    }

    #[trait_variant::make(Flush: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Flush`]
    pub trait LocalFlush: Transmit + Send + Sized {
        async fn flush(&mut self) -> Result<(), Self::Error>;
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
pub use async_variants::Flush as AsyncFlush;
pub use async_variants::Receive as AsyncReceive;
pub use async_variants::Splitable as AsyncSplitable;
pub use async_variants::Transmit as AsyncTransmit;

mod macros {
    macro_rules! delegate_transport_rx_sync {
        ($type:ident, $error:ty) => {
            impl crate::transport::Receive for $type {
                type Error = $error;
                fn receive(&mut self) -> Result<Bytes, $error> {
                    AsyncReceive::receive(&mut self.transport).syncify(&self.async_rt)
                }
            }
        };
    }

    macro_rules! delegate_transport_timeout_rx_sync {
        ($type:ident) => {
            impl crate::transport::TimeoutReceive for $type {
                fn receive_timeout(
                    &mut self,
                    timeout: std::time::Duration,
                ) -> Result<Option<Bytes>, Self::Error> {
                    let rt = self.async_rt.clone();
                    let res = (|| async move {
                        tokio::time::timeout(timeout, AsyncReceive::receive(&mut self.transport))
                            .await
                    })
                    .syncify_lazy(&rt);
                    match res {
                        Ok(Ok(bytes)) => Ok(Some(bytes)),
                        Ok(Err(err)) => Err(err),
                        Err(_) => Ok(None),
                    }
                }
            }
        };
    }

    macro_rules! delegate_transport_rx_async {
        ($type:ident, $error:ty) => {
            impl crate::transport::AsyncReceive for $type {
                type Error = $error;
                async fn receive(&mut self) -> Result<Bytes, $error> {
                    AsyncReceive::receive(&mut self.transport).await
                }
            }
        };
    }

    macro_rules! delegate_transport_tx_sync {
        ($type:ident, $error:ty) => {
            impl crate::transport::Transmit for $type {
                type Error = $error;
                fn transmit(&mut self, data: Bytes) -> Result<(), $error> {
                    AsyncTransmit::transmit(&mut self.transport, data).syncify(&self.async_rt)
                }
            }
        };
    }

    macro_rules! delegate_transport_tx_async {
        ($type:ident, $error:ty) => {
            impl crate::transport::AsyncTransmit for $type {
                type Error = $error;
                async fn transmit(&mut self, data: Bytes) -> Result<(), $error> {
                    AsyncTransmit::transmit(&mut self.transport, data).await
                }
            }
        };
    }

    macro_rules! delegate_transport_flush_sync {
        ($type:ident, $error:ty) => {
            impl crate::transport::Flush for $type {
                fn flush(&mut self) -> Result<(), $error> {
                    AsyncFlush::flush(&mut self.transport).syncify(&self.async_rt)
                }
            }
        };
    }

    macro_rules! delegate_transport_flush_async {
        ($type:ident, $error:ty) => {
            impl crate::transport::AsyncFlush for $type {
                async fn flush(&mut self) -> Result<(), $error> {
                    AsyncFlush::flush(&mut self.transport).await
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
        delegate_transport_flush_async, delegate_transport_flush_sync, delegate_transport_rx_async,
        delegate_transport_rx_sync, delegate_transport_split_async, delegate_transport_split_sync,
        delegate_transport_timeout_rx_sync, delegate_transport_tx_async,
        delegate_transport_tx_sync,
    };
}
