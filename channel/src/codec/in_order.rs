//! Reliable, in-order, message-oriented API.
//!
//! Built on-top of the [`crate::transport::stream`] API, this provides methods for sending and
//! receiving messages reliably and in-order.

use crate::transport::stream;
use crate::{AsyncReceiver, AsyncSender, Receiver, Sender};

use aperturec_protocol::{control, event};

use prost::Message;
use std::error::Error;
use std::io::{Read, Write};
use std::marker::PhantomData;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn encode<SM: Message>(msg: SM, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
    let msg_len = msg.encoded_len();
    let delim_len = prost::length_delimiter_len(msg_len);
    let nbytes = msg_len + delim_len;

    if buf.len() < nbytes {
        buf.resize(nbytes, 0);
    }
    msg.encode_length_delimited(&mut &mut buf[..])?;
    Ok(nbytes)
}

mod sync_impls {
    use super::*;

    pub(super) fn receive<T: Read, RM: Message + Default>(
        mut transport: T,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<(RM, usize)> {
        if buf.is_empty() {
            buf.resize(1, 0);
            transport.read_exact(&mut buf[..])?;
        }

        let msg_len = loop {
            match prost::decode_length_delimiter(&**buf) {
                Ok(delim) => break delim,
                Err(_) => {
                    let cur_len = buf.len();
                    buf.resize(cur_len + 1, 0);
                    transport.read_exact(&mut buf[cur_len..])?;
                }
            }
        };

        let delim_len = prost::length_delimiter_len(msg_len);
        let total_len = delim_len + msg_len;
        buf.resize(total_len, 0);
        transport.read_exact(&mut buf[delim_len..])?;

        let msg = RM::decode(&buf[delim_len..])?;
        buf.clear();
        Ok((msg, total_len))
    }

    pub(super) fn send<T: Write, SM: Message>(
        transport: &mut T,
        msg: SM,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let nbytes = encode(msg, buf)?;
        transport.write_all(&buf[..nbytes])?;
        Ok(())
    }
}

mod async_impls {
    use super::*;

    pub async fn receive<T: AsyncRead + Unpin, RM: Message + Default>(
        mut transport: T,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<(RM, usize)> {
        if buf.is_empty() {
            buf.resize(1, 0);
            transport.read_exact(&mut buf[..]).await?;
        }

        let msg_len = loop {
            match prost::decode_length_delimiter(&**buf) {
                Ok(delim) => break delim,
                Err(_) => {
                    let cur_len = buf.len();
                    buf.resize(cur_len + 1, 0);
                    transport.read_exact(&mut buf[cur_len..]).await?;
                }
            }
        };

        let delim_len = prost::length_delimiter_len(msg_len);
        let total_len = delim_len + msg_len;
        buf.resize(total_len, 0);
        transport.read_exact(&mut buf[delim_len..]).await?;

        let msg = RM::decode(&buf[delim_len..])?;
        buf.clear();
        Ok((msg, total_len))
    }

    pub async fn send<T: AsyncWrite + Unpin, SM: Message>(
        mut transport: T,
        msg: SM,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let nbytes = encode(msg, buf)?;
        transport.write_all(&buf[..nbytes]).await?;
        Ok(())
    }
}

/// Receive-only channel
#[derive(Debug)]
pub struct ReceiverSimplex<T: Read, ApiRm, WireRm> {
    transport: T,
    receive_buf: Vec<u8>,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: Read, ApiRm, WireRm> ReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        ReceiverSimplex {
            transport,
            receive_buf: vec![],
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T, ApiRm, WireRm> Receiver for ReceiverSimplex<T, ApiRm, WireRm>
where
    T: Read,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;

    fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        let (msg, msg_len) =
            sync_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf)?;
        Ok((msg.try_into()?, msg_len))
    }
}

/// Async variant of [`ReceiverSimplex`]
#[derive(Debug)]
pub struct AsyncReceiverSimplex<T: AsyncRead, ApiRm, WireRm> {
    transport: T,
    receive_buf: Vec<u8>,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: AsyncRead, ApiRm, WireRm> AsyncReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncReceiverSimplex {
            transport,
            receive_buf: vec![],
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<T, ApiRm, WireRm>
where
    T: AsyncRead + Unpin + Send + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiRm;

    async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
        let (msg, msg_len) =
            async_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf).await?;
        Ok((msg.try_into()?, msg_len))
    }
}

/// Send-only channel
#[derive(Debug)]
pub struct SenderSimplex<T: Write, ApiSm, WireSm> {
    transport: T,
    send_buf: Vec<u8>,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: Write, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        SenderSimplex {
            transport,
            send_buf: vec![],
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T, ApiSm, WireSm> Sender for SenderSimplex<T, ApiSm, WireSm>
where
    T: Write,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
{
    type Message = ApiSm;

    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        sync_impls::send::<T, WireSm>(&mut self.transport, msg.try_into()?, &mut self.send_buf)
    }
}

/// Async variant of [`SenderSimplex`]
#[derive(Debug)]
pub struct AsyncSenderSimplex<T: AsyncWrite, ApiSm, WireSm> {
    transport: T,
    send_buf: Vec<u8>,
    _api_sm: PhantomData<ApiSm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: AsyncWrite, ApiSm, WireSm> AsyncSenderSimplex<T, ApiSm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncSenderSimplex {
            transport,
            send_buf: vec![],
            _api_sm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<T, ApiSm, WireSm>
where
    T: AsyncWrite + Unpin + Send + 'static,
    WireSm: Message + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
{
    type Message = ApiSm;

    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        async_impls::send::<_, WireSm>(&mut self.transport, msg.try_into()?, &mut self.send_buf)
            .await
    }
}

/// Receive and send channel
#[derive(Debug)]
pub struct Duplex<T: Read + Write, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    receive_buf: Vec<u8>,
    send_buf: Vec<u8>,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: Read + Write, ApiRm, ApiSm, WireRm, WireSm> Duplex<T, ApiRm, ApiSm, WireRm, WireSm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        Duplex {
            transport,
            receive_buf: vec![],
            send_buf: vec![],
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

type DuplexReceiveHalf<T, ApiRm, WireRm> =
    ReceiverSimplex<<T as stream::Splitable>::ReceiveHalf, ApiRm, WireRm>;
type DuplexSendHalf<T, ApiSm, WireSm> =
    SenderSimplex<<T as stream::Splitable>::TransmitHalf, ApiSm, WireSm>;

impl<T: Read + Write + stream::Splitable, ApiRm, ApiSm, WireRm, WireSm>
    Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    pub fn split(
        self,
    ) -> (
        DuplexReceiveHalf<T, ApiRm, WireRm>,
        DuplexSendHalf<T, ApiSm, WireSm>,
    ) {
        let (rh, wh) = self.transport.split();

        (
            ReceiverSimplex {
                transport: rh,
                receive_buf: self.receive_buf,
                _api_rm: PhantomData,
                _wire_rm: PhantomData,
            },
            SenderSimplex {
                transport: wh,
                send_buf: self.send_buf,
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            },
        )
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: Read + Write,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    <ApiRm as TryFrom<WireRm>>::Error: Send + Sync + Error + 'static,
{
    type Message = ApiRm;
    fn receive_with_len(&mut self) -> anyhow::Result<(ApiRm, usize)> {
        let (msg, msg_len) =
            sync_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf)?;
        Ok((msg.try_into()?, msg_len))
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: Read + Write,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
    <ApiSm as TryInto<WireSm>>::Error: Send + Sync + Error + 'static,
{
    type Message = ApiSm;
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        sync_impls::send::<T, WireSm>(&mut self.transport, msg.try_into()?, &mut self.send_buf)
    }
}

/// Async variant of [`Duplex`]
#[derive(Debug)]
pub struct AsyncDuplex<T: AsyncRead + AsyncWrite, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    receive_buf: Vec<u8>,
    send_buf: Vec<u8>,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

/// Send half of a split [`AsyncDuplex`]
pub type AsyncDuplexSendHalf<T, ApiSm, WireSm> =
    AsyncSenderSimplex<<T as stream::AsyncSplitable>::TransmitHalf, ApiSm, WireSm>;
/// Receive half of a split [`AsyncDuplex`]
pub type AsyncDuplexReceiveHalf<T, ApiRm, WireRm> =
    AsyncReceiverSimplex<<T as stream::AsyncSplitable>::ReceiveHalf, ApiRm, WireRm>;

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: AsyncRead + AsyncWrite + stream::AsyncSplitable + Unpin,
{
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncDuplex {
            transport,
            receive_buf: vec![],
            send_buf: vec![],
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: AsyncRead + AsyncWrite + stream::AsyncSplitable + Unpin,
{
    /// Split into [`Sender`] and [`Receiver`] simplexes
    pub fn split(
        self,
    ) -> (
        AsyncDuplexReceiveHalf<T, ApiRm, WireRm>,
        AsyncDuplexSendHalf<T, ApiSm, WireSm>,
    ) {
        let (rh, wh) = self.transport.split();
        (AsyncReceiverSimplex::new(rh), AsyncSenderSimplex::new(wh))
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncReceiver for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    WireRm: Message + Default + Send + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    <ApiRm as TryFrom<WireRm>>::Error: Send + Sync + Error + 'static,
    ApiSm: Send + 'static,
    WireSm: Send + 'static,
{
    type Message = ApiRm;
    async fn receive_with_len(&mut self) -> anyhow::Result<(ApiRm, usize)> {
        let (msg, msg_len) =
            async_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf).await?;
        Ok((msg.try_into()?, msg_len))
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    WireSm: Message + Send + 'static,
    ApiSm: TryInto<WireSm> + Send + 'static,
    <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    WireRm: Message + Send + 'static,
{
    type Message = ApiSm;
    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
        async_impls::send(&mut self.transport, msg.try_into()?, &mut self.send_buf).await
    }
}

pub type ServerControlChannel = Duplex<
    stream::Transceiver,
    control::client_to_server::Message,
    control::server_to_client::Message,
    control::ClientToServer,
    control::ServerToClient,
>;
pub type ServerControlChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as stream::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;

pub type ServerControlChannelSendHalf = SenderSimplex<
    <stream::Transceiver as stream::Splitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;

pub type ClientControlChannel = Duplex<
    stream::Transceiver,
    control::server_to_client::Message,
    control::client_to_server::Message,
    control::ServerToClient,
    control::ClientToServer,
>;

pub type ClientControlChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as stream::Splitable>::ReceiveHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
pub type ClientControlChannelSendHalf = SenderSimplex<
    <stream::Transceiver as stream::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;

pub type ServerEventChannel = Duplex<
    stream::Transceiver,
    event::client_to_server::Message,
    event::server_to_client::Message,
    event::ClientToServer,
    event::ServerToClient,
>;
pub type ServerEventChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as stream::Splitable>::ReceiveHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
pub type ServerEventChannelSendHalf = SenderSimplex<
    <stream::Transceiver as stream::Splitable>::TransmitHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;

pub type ClientEventChannel = Duplex<
    stream::Transceiver,
    event::server_to_client::Message,
    event::client_to_server::Message,
    event::ServerToClient,
    event::ClientToServer,
>;
pub type ClientEventChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as stream::Splitable>::ReceiveHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
pub type ClientEventChannelSendHalf = SenderSimplex<
    <stream::Transceiver as stream::Splitable>::TransmitHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;

pub type AsyncServerControlChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    control::client_to_server::Message,
    control::server_to_client::Message,
    control::ClientToServer,
    control::ServerToClient,
>;
pub type AsyncServerControlChannelReceiveHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;
pub type AsyncServerControlChannelSendHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;

pub type AsyncClientControlChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    control::server_to_client::Message,
    control::client_to_server::Message,
    control::ServerToClient,
    control::ClientToServer,
>;
pub type AsyncClientControlChannelReceiveHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
pub type AsyncClientControlChannelSendHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;

pub type AsyncServerEventChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    event::client_to_server::Message,
    event::server_to_client::Message,
    event::ClientToServer,
    event::ServerToClient,
>;
pub type AsyncServerEventChannelReceiveHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::ReceiveHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
pub type AsyncServerEventChannelSendHalf = SenderSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;

pub type AsyncClientEventChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    event::server_to_client::Message,
    event::client_to_server::Message,
    event::ServerToClient,
    event::ClientToServer,
>;
pub type AsyncClientEventChannelReceiveHalf = ReceiverSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::ReceiveHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
pub type AsyncClientEventChannelSendHalf = SenderSimplex<
    <stream::AsyncTransceiver as stream::Splitable>::TransmitHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
