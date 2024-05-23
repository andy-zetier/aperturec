//! Byte-oriented, unreliable, out-of-order transport
use crate::provider::datagram::{ReceiverHandle, SenderHandle};
use crate::*;

use aperturec_trace::log;

use bytes::Bytes;
use s2n_quic::provider::event;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

static MAX_MTU: AtomicUsize = AtomicUsize::new(1200);

pub fn max_mtu() -> usize {
    MAX_MTU.load(Ordering::Relaxed)
}

pub(crate) struct MtuEventSubscriber;

#[derive(Default)]
pub(crate) struct MtuEventContext {
    path_mtus: BTreeMap<u64, usize>,
}

impl event::Subscriber for MtuEventSubscriber {
    type ConnectionContext = MtuEventContext;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        MtuEventContext::default()
    }

    fn on_mtu_updated(
        &mut self,
        ctx: &mut Self::ConnectionContext,
        _: &event::ConnectionMeta,
        event: &event::events::MtuUpdated,
    ) {
        ctx.path_mtus.insert(event.path_id, event.mtu as usize);
        let new_mtu = ctx.path_mtus.values().min().expect("no tracked MTUs");
        log::debug!(
            "New MTU: {} on path {} with cause {:?}",
            *new_mtu,
            event.path_id,
            event.cause
        );
        MAX_MTU.store(*new_mtu, Ordering::Relaxed);
    }
}

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

    /// Get a [`Gated`] version of [`Self`] which is rate limited by the provided [`Gate`]
    fn gated<G: Gate>(self, gate: G) -> Gated<Self, G> {
        Gated {
            ungated: self,
            gate,
        }
    }
}

/// A [`Gated`] datagram [`Transmit`]er
///
/// This is constructed by calling [`Transmit::gated`] on an existing [`Transmit`] type
pub struct Gated<T: Transmit, G: Gate> {
    ungated: T,
    gate: G,
}

impl<T: Transmit, G: Gate> Transmit for Gated<T, G> {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        self.gate.wait_for_permission::<usize>(data.len())?;
        self.ungated.transmit(data)
    }
}

impl<R: Transmit + Receive, G: Gate> Receive for Gated<R, G> {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        self.ungated.receive()
    }
}

mod async_variants {
    use crate::AsyncGate as Gate;

    use bytes::Bytes;

    /// Async variant of [`Receive`](super::Receive)
    #[trait_variant::make(Receive: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalReceive {
        async fn receive(&mut self) -> anyhow::Result<Bytes>;
    }

    /// Async variant of [`Transmit`](super::Transmit)
    #[trait_variant::make(Transmit: Send + Sync)]
    pub trait LocalTransmit: Sized {
        async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()>;

        fn gated<G: Gate>(self, gate: G) -> Gated<Self, G> {
            Gated {
                ungated: self,
                gate,
            }
        }
    }

    /// Async variant of [`Gated`](super::Gated)
    pub struct Gated<T: LocalTransmit, G: Gate> {
        ungated: T,
        gate: G,
    }

    impl<T: Transmit, G: Gate> Transmit for Gated<T, G> {
        async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
            self.gate.wait_for_permission::<usize>(data.len()).await?;
            self.ungated.transmit(data).await
        }
    }

    impl<R: Transmit + Receive, G: Gate> Receive for Gated<R, G> {
        async fn receive(&mut self) -> anyhow::Result<Bytes> {
            self.ungated.receive().await
        }
    }
}
pub use async_variants::{Gated as AsyncGated, Receive as AsyncReceive, Transmit as AsyncTransmit};

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
    handle: SenderHandle,
}

impl Transmitter {
    /// Create a new [`Self`]
    pub fn new(handle: SenderHandle) -> Self {
        Transmitter { handle }
    }
}

impl Transmit for Transmitter {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        Transmit::transmit(&mut self.handle, data)
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
