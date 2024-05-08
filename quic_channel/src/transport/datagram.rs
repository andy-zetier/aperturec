//! Byte-oriented, unreliable, out-of-order transport
use crate::util::Syncify;
use crate::*;

use anyhow::anyhow;
use bytes::Bytes;
use futures::future;
use s2n_quic::provider::datagram::default::{Receiver as QuicReceiver, Sender as QuicTransmitter};
use s2n_quic::provider::event;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::task::Poll;
use tokio::runtime::Runtime as TokioRuntime;

pub(crate) const SEND_QUEUE_SIZE: usize = 1;
pub(crate) const RECV_QUEUE_SIZE: usize = 2_usize.pow(16);

const DEFAULT_MTU: usize = 1000;
static MAX_MTU: RwLock<usize> = RwLock::new(DEFAULT_MTU);

pub fn max_mtu() -> usize {
    *MAX_MTU.read().expect("read")
}

pub(crate) struct MtuEventSubscriber;

#[derive(Default)]
pub(crate) struct MtuEventContext {
    path_mtus: BTreeMap<u64, u16>,
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
        ctx.path_mtus.entry(event.path_id).or_insert(event.mtu);
        let min = ctx.path_mtus.values().min().expect("no tracked MTUs");
        *MAX_MTU.write().expect("write") = *min as usize;
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

mod async_impls {
    use super::*;

    pub(super) async fn receive(
        handle: &mut s2n_quic::connection::Handle,
    ) -> anyhow::Result<Bytes> {
        future::poll_fn(|cx| {
            match handle.datagram_mut(|rx: &mut QuicReceiver| rx.poll_recv_datagram(cx)) {
                Ok(poll_value) => poll_value.map(Ok),
                Err(e) => Poll::Ready(Err(e)),
            }
        })
        .await?
        .map_err(|e| anyhow!(e))
    }

    pub(super) async fn transmit(
        handle: &mut s2n_quic::connection::Handle,
        mut data: Bytes,
    ) -> anyhow::Result<()> {
        future::poll_fn(|cx| {
            match handle
                .datagram_mut(|tx: &mut QuicTransmitter| tx.poll_send_datagram(&mut data, cx))
            {
                Ok(poll_value) => poll_value.map(Ok),
                Err(e) => Poll::Ready(Err(e)),
            }
        })
        .await?
        .map_err(|e| anyhow!(e))
    }
}

mod sync_impls {
    use super::*;

    pub(super) fn receive(
        handle: &mut s2n_quic::connection::Handle,
        async_rt: &TokioRuntime,
    ) -> anyhow::Result<Bytes> {
        async_impls::receive(handle).syncify(async_rt)
    }

    pub(super) fn transmit(
        handle: &mut s2n_quic::connection::Handle,
        data: Bytes,
        async_rt: &TokioRuntime,
    ) -> anyhow::Result<()> {
        async_impls::transmit(handle, data).syncify(async_rt)
    }
}

/// A byte-oriented sender and receiver
#[derive(Debug, Clone)]
pub struct Transceiver {
    handle: s2n_quic::connection::Handle,
    async_rt: Arc<TokioRuntime>,
}

impl Transceiver {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle, async_rt: Arc<TokioRuntime>) -> Self {
        Transceiver { handle, async_rt }
    }
}

impl Receive for Transceiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        sync_impls::receive(&mut self.handle, &self.async_rt)
    }
}

impl Transmit for Transceiver {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        sync_impls::transmit(&mut self.handle, data, &self.async_rt)
    }
}

/// A byte-oriented sender
#[derive(Debug, Clone)]
pub struct Transmitter {
    handle: s2n_quic::connection::Handle,
    async_rt: Arc<TokioRuntime>,
}

impl Transmitter {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle, async_rt: Arc<TokioRuntime>) -> Self {
        Transmitter { handle, async_rt }
    }
}

impl Transmit for Transmitter {
    fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        sync_impls::transmit(&mut self.handle, data, &self.async_rt)
    }
}

/// A byte-oriented receiver
#[derive(Debug, Clone)]
pub struct Receiver {
    handle: s2n_quic::connection::Handle,
    async_rt: Arc<TokioRuntime>,
}

impl Receiver {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle, async_rt: Arc<TokioRuntime>) -> Self {
        Receiver { handle, async_rt }
    }
}

impl Receive for Receiver {
    fn receive(&mut self) -> anyhow::Result<Bytes> {
        sync_impls::receive(&mut self.handle, &self.async_rt)
    }
}

/// Async variant of [`Transceiver`]
#[derive(Clone, Debug)]
pub struct AsyncTransceiver {
    handle: s2n_quic::connection::Handle,
}

impl AsyncTransceiver {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle) -> Self {
        AsyncTransceiver { handle }
    }
}

impl AsyncReceive for AsyncTransceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        async_impls::receive(&mut self.handle).await
    }
}

impl AsyncTransmit for AsyncTransceiver {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        async_impls::transmit(&mut self.handle, data).await
    }
}

/// Async variant of [`Transmitter`]
#[derive(Debug, Clone)]
pub struct AsyncTransmitter {
    handle: s2n_quic::connection::Handle,
}

impl AsyncTransmitter {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle) -> Self {
        AsyncTransmitter { handle }
    }
}

impl AsyncTransmit for AsyncTransmitter {
    async fn transmit(&mut self, data: Bytes) -> anyhow::Result<()> {
        async_impls::transmit(&mut self.handle, data).await
    }
}

/// Async variant of [`Receiver`]
#[derive(Debug, Clone)]
pub struct AsyncReceiver {
    handle: s2n_quic::connection::Handle,
}

impl AsyncReceiver {
    /// Create a new [`Self`]
    pub fn new(handle: s2n_quic::connection::Handle) -> Self {
        AsyncReceiver { handle }
    }
}

impl AsyncReceive for AsyncReceiver {
    async fn receive(&mut self) -> anyhow::Result<Bytes> {
        async_impls::receive(&mut self.handle).await
    }
}
