//! Unreliable, message-oriented API
//!
//! Built on-top of the [`crate::transport::datagram`] API, this provides methods for sending and
//! receiving messages unreliably and out-of-order.

use crate::gate::*;
use crate::transport::datagram::{self, AsyncReceive, AsyncTransmit, Receive, Transmit};
use crate::*;

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
    <G as Gate>::StampedMessage: Into<<S as Sender>::Message>,
{
    type Message = <G as Gate>::UnstampedMessage;

    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        let stamped = self.gate.wait_and_stamp(msg)?;
        self.ungated.send(stamped.into())
    }
}

impl<S: AsyncSender, G: AsyncGate + Send> AsyncSender for Gated<S, G>
where
    <G as AsyncGate>::StampedMessage: Into<<S as AsyncSender>::Message>,
    <G as AsyncGate>::UnstampedMessage: Send,
{
    type Message = <G as AsyncGate>::UnstampedMessage;

    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        let stamped = self.gate.wait_and_stamp(msg).await?;
        self.ungated.send(stamped.into()).await
    }
}

fn encode<ApiSm, WireSm>(msg: ApiSm) -> anyhow::Result<Bytes>
where
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    Ok(Bytes::from(msg.try_into()?.encode_to_vec()))
}

fn decode<ApiRm, WireRm>(dg: Bytes) -> anyhow::Result<(ApiRm, usize)>
where
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    let dg_len = dg.len();
    let msg = WireRm::decode(dg)?.try_into()?;
    Ok((msg, dg_len))
}

pub(crate) mod sync_impls {
    use super::*;

    pub(crate) fn do_send<T, ApiSm, WireSm>(dg_transport: &mut T, msg: ApiSm) -> anyhow::Result<()>
    where
        T: Transmit,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = encode(msg)?;
        dg_transport.transmit(dg)?;
        Ok(())
    }

    pub(crate) fn do_receive<T, ApiRm, WireRm>(
        dg_transport: &mut T,
    ) -> anyhow::Result<(ApiRm, usize)>
    where
        T: Receive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = dg_transport.receive()?;
        decode(dg)
    }
}

pub(crate) mod async_impls {
    use super::*;

    pub(crate) async fn do_send<T, ApiSm, WireSm>(
        dg_transport: &mut T,
        msg: ApiSm,
    ) -> anyhow::Result<()>
    where
        T: AsyncTransmit,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = encode(msg)?;
        dg_transport.transmit(dg).await?;
        Ok(())
    }

    pub(crate) async fn do_receive<T, ApiRm, WireRm>(
        dg_transport: &mut T,
    ) -> anyhow::Result<(ApiRm, usize)>
    where
        T: AsyncReceive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        let dg = dg_transport.receive().await?;
        decode(dg)
    }
}

/// Receive-only channel
#[derive(Debug)]
pub struct ReceiverSimplex<T: Receive, ApiRm, WireRm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: Receive, ApiRm, WireRm> ReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        ReceiverSimplex {
            transport,
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T, ApiRm, WireRm> Receiver for ReceiverSimplex<T, ApiRm, WireRm>
where
    T: Receive,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;
    fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        sync_impls::do_receive(&mut self.transport)
    }
}

/// Async variant of [`ReceiverSimplex`]
#[derive(Debug)]
pub struct AsyncReceiverSimplex<T: AsyncReceive, ApiRm, WireRm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: AsyncReceive, ApiRm, WireRm> AsyncReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncReceiverSimplex {
            transport,
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<T, ApiRm, WireRm>
where
    T: AsyncReceive + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
{
    type Message = ApiRm;

    async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        async_impls::do_receive(&mut self.transport).await
    }
}

/// Send-only channel
#[derive(Debug, Clone)]
pub struct SenderSimplex<T: Transmit, ApiSm, WireSm> {
    transport: T,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: Transmit, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        SenderSimplex {
            transport,
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T: Transmit, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm>
where
    T: Transmit,
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
    T: Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiSm;
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        sync_impls::do_send(&mut self.transport, msg)
    }
}

/// Async variant of [`SenderSimplex`]
#[derive(Debug, Clone)]
pub struct AsyncSenderSimplex<T: AsyncTransmit, ApiSm, WireSm> {
    transport: T,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: AsyncTransmit, ApiSm, WireSm> AsyncSenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncSenderSimplex {
            transport,
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
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
    T: AsyncTransmit + 'static,
    WireSm: Message + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
{
    type Message = ApiSm;

    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        async_impls::do_send(&mut self.transport, msg).await
    }
}

/// Receive and send channel
#[derive(Debug)]
pub struct Duplex<T: Receive + Transmit, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: Receive + Transmit, ApiRm, ApiSm, WireRm, WireSm> Duplex<T, ApiRm, ApiSm, WireRm, WireSm> {
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
    T: Receive + Transmit,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;
    fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        sync_impls::do_receive(&mut self.transport)
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: Receive + Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiSm;
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        sync_impls::do_send(&mut self.transport, msg)
    }
}

/// Async variant of [`Duplex`]
#[derive(Debug)]
pub struct AsyncDuplex<T: AsyncReceive + AsyncTransmit, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: AsyncReceive + AsyncTransmit, ApiRm, ApiSm, WireRm, WireSm>
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
    T: AsyncReceive + AsyncTransmit + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
    WireSm: Send + 'static,
    ApiSm: Send + 'static,
{
    type Message = ApiRm;

    async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        async_impls::do_receive(&mut self.transport).await
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: AsyncReceive + AsyncTransmit + 'static,
    WireSm: Message + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
    WireRm: Send + 'static,
    ApiRm: Send + 'static,
{
    type Message = ApiSm;

    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        async_impls::do_send(&mut self.transport, msg).await
    }
}

pub type ClientMediaChannel =
    ReceiverSimplex<datagram::Receiver, media::server_to_client::Message, media::ServerToClient>;

pub type AsyncClientMediaChannel = AsyncReceiverSimplex<
    datagram::AsyncReceiver,
    media::server_to_client::Message,
    media::ServerToClient,
>;

pub type ServerMediaChannel =
    SenderSimplex<datagram::Transmitter, media::server_to_client::Message, media::ServerToClient>;
pub type AsyncServerMediaChannel = AsyncSenderSimplex<
    datagram::AsyncTransmitter,
    media::server_to_client::Message,
    media::ServerToClient,
>;

pub type GatedServerMediaChannel<G> = Gated<
    SenderSimplex<datagram::Transmitter, media::server_to_client::Message, media::ServerToClient>,
    G,
>;
pub type AsyncGatedServerMediaChannel<G> = Gated<
    AsyncSenderSimplex<
        datagram::AsyncTransmitter,
        media::server_to_client::Message,
        media::ServerToClient,
    >,
    G,
>;
