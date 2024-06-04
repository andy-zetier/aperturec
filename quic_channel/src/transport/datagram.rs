//! Byte-oriented, unreliable, out-of-order transport
use crate::provider::datagram::{ReceiverHandle, SenderHandle};

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
    #[trait_variant::make(Receive: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalReceive {
        async fn receive(&mut self) -> anyhow::Result<Bytes>;
    }

    /// Async variant of [`Transmit`](super::Transmit)
    #[trait_variant::make(Transmit: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalTransmit: Sized {
        async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()>;
    }
}
pub use async_variants::{Receive as AsyncReceive, Transmit as AsyncTransmit};

/// A byte-oriented sender and receiver
#[derive(Debug)]
pub struct Transceiver {
    sender_handle: SenderHandle,
    receiver_handle: ReceiverHandle,
}

impl Transceiver {
    /// Create a new [`Self`]
    pub fn new(sender_handle: SenderHandle, receiver_handle: ReceiverHandle) -> Self {
        Transceiver {
            sender_handle,
            receiver_handle,
        }
    }
}

impl Receive for Transceiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        Receive::receive(&mut self.receiver_handle)
    }
}

impl Transmit for Transceiver {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        Transmit::transmit(&mut self.sender_handle, data)
    }
}

/// A byte-oriented sender
#[derive(Debug, Clone)]
pub struct Transmitter {
    sender_handle: SenderHandle,
}

impl Transmitter {
    /// Create a new [`Self`]
    pub fn new(sender_handle: SenderHandle) -> Self {
        Transmitter { sender_handle }
    }
}

impl Transmit for Transmitter {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        Transmit::transmit(&mut self.sender_handle, data)
    }
}

/// A byte-oriented receiver
#[derive(Debug)]
pub struct Receiver {
    handle: ReceiverHandle,
}

impl Receiver {
    /// Create a new [`Self`]
    pub fn new(handle: ReceiverHandle) -> Self {
        Receiver { handle }
    }
}

impl Receive for Receiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        Receive::receive(&mut self.handle)
    }
}

/// Async variant of [`Transceiver`]
#[derive(Debug)]
pub struct AsyncTransceiver {
    sender_handle: SenderHandle,
    receiver_handle: ReceiverHandle,
}

impl AsyncTransceiver {
    /// Create a new [`Self`]
    pub fn new(sender_handle: SenderHandle, receiver_handle: ReceiverHandle) -> Self {
        AsyncTransceiver {
            sender_handle,
            receiver_handle,
        }
    }
}

impl AsyncReceive for AsyncTransceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.receiver_handle).await
    }
}

impl AsyncTransmit for AsyncTransceiver {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.sender_handle, data).await
    }
}

/// Async variant of [`Transmitter`]
#[derive(Debug, Clone)]
pub struct AsyncTransmitter {
    sender_handle: SenderHandle,
}

impl AsyncTransmitter {
    /// Create a new [`Self`]
    pub fn new(sender_handle: SenderHandle) -> Self {
        AsyncTransmitter { sender_handle }
    }
}

impl AsyncTransmit for AsyncTransmitter {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        AsyncTransmit::transmit(&mut self.sender_handle, data).await
    }
}

/// Async variant of [`Receiver`]
#[derive(Debug)]
pub struct AsyncReceiver {
    receiver_handle: ReceiverHandle,
}

impl AsyncReceiver {
    /// Create a new [`Self`]
    pub fn new(receiver_handle: ReceiverHandle) -> Self {
        AsyncReceiver { receiver_handle }
    }
}

impl AsyncReceive for AsyncReceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        AsyncReceive::receive(&mut self.receiver_handle).await
    }
}
