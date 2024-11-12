//! Reliable, out-of-order, message-oriented API
//!
//! Built on-top of the [`crate::transport::datagram`] API, this provides methods for sending and
//! receiving messages reliably and out-of-order.

use crate::gate::*;
use crate::transport::{self, datagram};

use super::*;

use aperturec_protocol::media;

use anyhow::Result;
use bytes::Bytes;
use prost::Message;
use std::error::Error;
use std::marker::PhantomData;

pub struct Gated<S, G> {
    gate: G,
    ungated: S,
}

impl<S: Sender, G: Gate> Sender for Gated<S, G>
where
    S::Message: Message,
{
    type Message = S::Message;

    fn send(&mut self, msg: Self::Message) -> Result<()> {
        let msg_size = msg.encoded_len();
        self.gate.wait(msg_size)?;
        self.ungated.send(msg)?;
        Ok(())
    }
}

impl<S: AsyncSender, G: AsyncGate + Send> AsyncSender for Gated<S, G>
where
    S::Message: Message,
{
    type Message = S::Message;

    async fn send(&mut self, msg: Self::Message) -> Result<()> {
        let msg_size = msg.encoded_len();
        self.gate.wait(msg_size).await?;
        self.ungated.send(msg).await?;
        Ok(())
    }
}

impl<S: AsyncFlushable, G: AsyncGate + Send> AsyncFlushable for Gated<S, G>
where
    S::Message: Message,
{
    async fn flush(&mut self) -> Result<()> {
        self.ungated.flush().await
    }
}

fn encode<ApiSm, WireSm>(msg: ApiSm) -> Result<Bytes>
where
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    Ok(Bytes::from(msg.try_into()?.encode_to_vec()))
}

fn decode<ApiRm, WireRm>(dg: Bytes) -> Result<(ApiRm, usize)>
where
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    let dg_len = dg.len();
    let msg = WireRm::decode(dg)?.try_into()?;
    Ok((msg, dg_len))
}

mod sync_impls {
    use super::*;

    pub(super) fn do_send<T, ApiSm, WireSm>(dg_transport: &mut T, msg: ApiSm) -> Result<()>
    where
        T: transport::Transmit,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = encode(msg)?;
        dg_transport.transmit(dg)?;
        Ok(())
    }

    pub(super) fn do_receive<T, ApiRm, WireRm>(dg_transport: &mut T) -> Result<(ApiRm, usize)>
    where
        T: transport::Receive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        decode(dg_transport.receive()?)
    }
}

mod async_impls {
    use super::*;

    pub(super) async fn do_send<T, ApiSm, WireSm>(dg_transport: &mut T, msg: ApiSm) -> Result<()>
    where
        T: transport::AsyncTransmit,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = encode(msg)?;
        dg_transport.transmit(dg).await?;
        Ok(())
    }

    pub(super) async fn do_receive<T, ApiRm, WireRm>(dg_transport: &mut T) -> Result<(ApiRm, usize)>
    where
        T: transport::AsyncReceive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        decode(dg_transport.receive().await?)
    }
}

/// Receive-only channel
#[derive(Debug)]
pub struct ReceiverSimplex<T: transport::Receive, ApiRm, WireRm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: transport::Receive, ApiRm, WireRm> ReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        ReceiverSimplex {
            transport,
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T, ApiRm, WireRm> Receiver for ReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::Receive,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;
    fn receive_with_len(&mut self) -> Result<(Self::Message, usize)> {
        sync_impls::do_receive(&mut self.transport)
    }
}

/// Async variant of [`ReceiverSimplex`]
#[derive(Debug)]
pub struct AsyncReceiverSimplex<T: transport::AsyncReceive, ApiRm, WireRm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: transport::AsyncReceive, ApiRm, WireRm> AsyncReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncReceiverSimplex {
            transport,
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::AsyncReceive + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
{
    type Message = ApiRm;

    async fn receive_with_len(&mut self) -> Result<(Self::Message, usize)> {
        async_impls::do_receive(&mut self.transport).await
    }
}

/// Send-only channel
#[derive(Debug, Clone)]
pub struct SenderSimplex<T: transport::Transmit, ApiSm, WireSm> {
    transport: T,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::Transmit, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        SenderSimplex {
            transport,
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T: transport::Transmit, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm>
where
    T: transport::Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    /// Create a [`Self`] which limits the rate at which messages are sent based on the
    /// provided [`Gate`]
    pub fn gated<G: Gate>(self, gate: G) -> Gated<Self, G> {
        Gated {
            gate,
            ungated: self,
        }
    }
}

impl<T, ApiSm, WireSm> Sender for SenderSimplex<T, ApiSm, WireSm>
where
    T: transport::Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiSm;
    fn send(&mut self, msg: Self::Message) -> Result<()> {
        sync_impls::do_send(&mut self.transport, msg)
    }
}

/// Async variant of [`SenderSimplex`]
#[derive(Debug, Clone)]
pub struct AsyncSenderSimplex<T: transport::AsyncTransmit, ApiSm, WireSm> {
    transport: T,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::AsyncTransmit, ApiSm, WireSm> AsyncSenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncSenderSimplex {
            transport,
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Create a [`Self`] which limits the rate at which messages are sent based on the
    /// provided [`Gate`]
    pub fn gated<G: AsyncGate>(self, gate: G) -> Gated<Self, G> {
        Gated {
            gate,
            ungated: self,
        }
    }
}

impl<T, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<T, ApiSm, WireSm>
where
    T: transport::AsyncTransmit + 'static,
    WireSm: Message + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
{
    type Message = ApiSm;

    async fn send(&mut self, msg: Self::Message) -> Result<()> {
        async_impls::do_send(&mut self.transport, msg).await
    }
}

/// Receive and send channel
#[derive(Debug)]
pub struct Duplex<T: transport::Receive + transport::Transmit, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::Receive + transport::Transmit, ApiRm, ApiSm, WireRm, WireSm>
    Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        Duplex {
            transport,
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Create a [`Self`] which limits the rate at which messages are sent based on the
    /// provided [`Gate`]
    pub fn gated<G: Gate>(self, gate: G) -> Gated<Self, G> {
        Gated {
            gate,
            ungated: self,
        }
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Transmit,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;
    fn receive_with_len(&mut self) -> Result<(Self::Message, usize)> {
        sync_impls::do_receive(&mut self.transport)
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiSm;
    fn send(&mut self, msg: Self::Message) -> Result<()> {
        sync_impls::do_send(&mut self.transport, msg)
    }
}

/// Async variant of [`Duplex`]
#[derive(Debug)]
pub struct AsyncDuplex<
    T: transport::AsyncReceive + transport::AsyncTransmit,
    ApiRm,
    ApiSm,
    WireRm,
    WireSm,
> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::AsyncReceive + transport::AsyncTransmit, ApiRm, ApiSm, WireRm, WireSm>
    AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncDuplex {
            transport,
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }

    /// Convert [`Self`] into the underlying transport
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Create a [`Self`] which limits the rate at which messages are sent based on the
    /// provided [`Gate`]
    pub fn gated<G: AsyncGate>(self, gate: G) -> Gated<Self, G> {
        Gated {
            gate,
            ungated: self,
        }
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncReceiver for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
    WireSm: Send + 'static,
    ApiSm: Send + 'static,
{
    type Message = ApiRm;

    async fn receive_with_len(&mut self) -> Result<(Self::Message, usize)> {
        async_impls::do_receive(&mut self.transport).await
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + 'static,
    WireSm: Message + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
    WireRm: Send + 'static,
    ApiRm: Send + 'static,
{
    type Message = ApiSm;

    async fn send(&mut self, msg: Self::Message) -> Result<()> {
        async_impls::do_send(&mut self.transport, msg).await
    }
}

// Client
// Sync
pub type ClientMediaChannel =
    ReceiverSimplex<datagram::Receiver, media::ServerToClient, media::ServerToClient>;
// Async
pub type AsyncClientMediaChannel =
    AsyncReceiverSimplex<datagram::AsyncReceiver, media::ServerToClient, media::ServerToClient>;
// Server
// Sync
pub type ServerMediaChannel =
    SenderSimplex<datagram::Transmitter, media::ServerToClient, media::ServerToClient>;
// Async
pub type AsyncServerMediaChannel =
    AsyncSenderSimplex<datagram::AsyncTransmitter, media::ServerToClient, media::ServerToClient>;
// Gated
// Server
// Sync
pub type GatedServerMediaChannel<G> =
    Gated<SenderSimplex<datagram::Transmitter, media::ServerToClient, media::ServerToClient>, G>;
// Async
pub type AsyncGatedServerMediaChannel<G> = Gated<
    AsyncSenderSimplex<datagram::AsyncTransmitter, media::ServerToClient, media::ServerToClient>,
    G,
>;
