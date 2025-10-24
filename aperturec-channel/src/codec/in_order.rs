//! Reliable, in-order, message-oriented API.
//!
//! Built on-top of the [`crate::transport::stream`] API, this provides methods for sending and
//! receiving messages reliably and in-order.

use super::{
    AsyncFlushable, AsyncReceiver, AsyncSender, Flushable, Receiver, Sender, TimeoutReceiver,
};
use crate::transport::{self, stream};

use aperturec_protocol::{control, event, tunnel};

use bytes::{Bytes, BytesMut};
use prost::Message;
use std::{
    convert::Infallible,
    marker::PhantomData,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

/// Errors that can occur when receiving messages over an in-order channel
#[derive(Debug, thiserror::Error)]
pub enum RxError {
    /// A transport-layer error occurred while receiving data
    #[error(transparent)]
    Transport(#[from] stream::RxError),

    /// Failed to decode the received message
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),

    /// A required field was missing in the received message
    #[error(transparent)]
    MissingField(#[from] aperturec_protocol::WrappedOptionalConvertError),
}

// Implemented to allow `?` on Infallible errors when returning Result<T, RxError>
impl From<Infallible> for RxError {
    fn from(_: Infallible) -> Self {
        unreachable!("Infallible being converted to an error");
    }
}

/// Errors that can occur when sending messages over an in-order channel
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// A transport-layer error occurred while sending data
    #[error(transparent)]
    Transport(#[from] stream::TxError),

    /// Failed to encode the message for transmission
    #[error(transparent)]
    Encode(#[from] prost::EncodeError),
}

fn encode<SM: Message>(msg: &SM) -> Result<Bytes, prost::EncodeError> {
    let msg_len = msg.encoded_len();
    let delim_len = prost::length_delimiter_len(msg_len);
    let nbytes = msg_len + delim_len;

    let mut bytes = BytesMut::with_capacity(nbytes);
    msg.encode_length_delimited(&mut bytes)?;
    Ok(bytes.freeze())
}

mod sync_impls {
    use super::*;

    /// Internal helper that implements the common receive logic for both blocking and timeout cases.
    ///
    /// The `recv_fn` closure should attempt to receive bytes, returning:
    /// - `Ok(Some(bytes))` when bytes are successfully received
    /// - `Ok(None)` when a timeout occurs (only applicable for timeout variant)
    /// - `Err(e)` when an error occurs
    fn receive_internal<RM, E, F>(
        buf: &mut Vec<u8>,
        mut recv_fn: F,
    ) -> Result<Option<(RM, usize)>, RxError>
    where
        RM: Message + Default,
        RxError: From<E>,
        F: FnMut() -> Result<Option<Bytes>, E>,
    {
        // Decode the length delimiter
        let mut delim_len = 0;
        let msg_len = loop {
            match prost::decode_length_delimiter(&buf[..delim_len]) {
                Ok(delim) => break delim,
                Err(_) => {
                    delim_len += 1;
                    if delim_len > buf.len() {
                        match recv_fn()? {
                            Some(bytes) => buf.extend(bytes),
                            None => return Ok(None),
                        }
                    }
                }
            }
        };

        // Receive remaining message bytes
        let total_len = delim_len + msg_len;
        while total_len > buf.len() {
            match recv_fn()? {
                Some(bytes) => buf.extend(bytes),
                None => return Ok(None),
            }
        }

        // Decode the message
        let remaining_len = buf.len() - total_len;
        let msg = RM::decode(&buf[delim_len..total_len])?;

        // Clean up the buffer
        buf.rotate_left(total_len);
        buf[remaining_len..].zeroize();
        buf.truncate(remaining_len);

        Ok(Some((msg, total_len)))
    }

    pub(super) fn receive_timeout<T: transport::TimeoutReceive, RM: Message + Default>(
        transport: &mut T,
        buf: &mut Vec<u8>,
        timeout: Duration,
    ) -> Result<Option<(RM, usize)>, RxError>
    where
        RxError: From<<T as transport::Receive>::Error>,
    {
        let start = Instant::now();
        let mut remaining_time = timeout;

        receive_internal(buf, || {
            let result = transport.receive_timeout(remaining_time)?;
            remaining_time = timeout.saturating_sub(Instant::now() - start);
            Ok(result)
        })
    }

    pub(super) fn receive<T: transport::Receive, RM: Message + Default>(
        transport: &mut T,
        buf: &mut Vec<u8>,
    ) -> Result<(RM, usize), RxError>
    where
        RxError: From<<T as transport::Receive>::Error>,
    {
        // Safe unwrap: blocking receive always returns Some, never None (timeout only applies to timeout variant)
        receive_internal(buf, || transport.receive().map(Some))
            .transpose()
            .unwrap()
    }

    pub(super) fn send<T: transport::Transmit, SM: Message>(
        transport: &mut T,
        msg: SM,
    ) -> Result<(), TxError>
    where
        TxError: From<<T as transport::Transmit>::Error>,
    {
        let bytes = encode(&msg)?;
        transport.transmit(bytes)?;
        Ok(())
    }
}

mod async_impls {
    use super::*;

    pub async fn receive<T: transport::AsyncReceive + Unpin, RM: Message + Default>(
        transport: &mut T,
        buf: &mut Vec<u8>,
    ) -> Result<(RM, usize), RxError>
    where
        RxError: From<<T as transport::AsyncReceive>::Error>,
    {
        let mut delim_len = 0;
        let msg_len = loop {
            match prost::decode_length_delimiter(&buf[..delim_len]) {
                Ok(delim) => break delim,
                Err(_) => {
                    delim_len += 1;
                    if delim_len > buf.len() {
                        buf.extend(transport.receive().await?);
                    }
                }
            }
        };

        let total_len = delim_len + msg_len;
        while total_len > buf.len() {
            buf.extend(transport.receive().await?);
        }
        let remaining_len = buf.len() - total_len;

        let msg = RM::decode(&buf[delim_len..total_len])?;

        buf.rotate_left(total_len);
        buf[remaining_len..].zeroize();
        buf.truncate(remaining_len);

        Ok((msg, total_len))
    }

    pub async fn send<T: transport::AsyncTransmit + Unpin, SM: Message>(
        transport: &mut T,
        msg: SM,
    ) -> Result<(), TxError>
    where
        TxError: From<<T as transport::AsyncTransmit>::Error>,
    {
        let bytes = encode(&msg)?;
        transport.transmit(bytes).await?;
        Ok(())
    }
}

/// Receive-only channel
#[derive(Debug)]
pub struct ReceiverSimplex<T: transport::Receive, ApiRm, WireRm> {
    transport: T,
    receive_buf: Zeroizing<Vec<u8>>,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: transport::Receive, ApiRm, WireRm> ReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        ReceiverSimplex {
            transport,
            receive_buf: Zeroizing::default(),
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T: transport::Receive, ApiRm, WireRm> AsRef<T> for ReceiverSimplex<T, ApiRm, WireRm> {
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, WireRm> Receiver for ReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::Receive,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
{
    type Message = ApiRm;
    type Error = RxError;

    fn receive_with_len(&mut self) -> Result<(Self::Message, usize), RxError> {
        let (msg, msg_len) =
            sync_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf)?;
        Ok((msg.try_into()?, msg_len))
    }
}

impl<T, ApiRm, WireRm> TimeoutReceiver for ReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::TimeoutReceive,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
{
    fn receive_with_len_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(Self::Message, usize)>, Self::Error> {
        if let Some((msg, msg_len)) = sync_impls::receive_timeout::<_, WireRm>(
            &mut self.transport,
            &mut self.receive_buf,
            timeout,
        )? {
            Ok(Some((msg.try_into()?, msg_len)))
        } else {
            Ok(None)
        }
    }
}

/// Async variant of [`ReceiverSimplex`]
#[derive(Debug)]
pub struct AsyncReceiverSimplex<T: transport::AsyncReceive, ApiRm, WireRm> {
    transport: T,
    receive_buf: Zeroizing<Vec<u8>>,
    _api_rm: PhantomData<ApiRm>,
    _wire_rm: PhantomData<WireRm>,
}

impl<T: transport::AsyncReceive, ApiRm, WireRm> AsyncReceiverSimplex<T, ApiRm, WireRm> {
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncReceiverSimplex {
            transport,
            receive_buf: Zeroizing::default(),
            _api_rm: PhantomData,
            _wire_rm: PhantomData,
        }
    }
}

impl<T: transport::AsyncReceive, ApiRm, WireRm> AsRef<T>
    for AsyncReceiverSimplex<T, ApiRm, WireRm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::AsyncReceive + Unpin + Send + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send,
    RxError: From<<WireRm as TryInto<ApiRm>>::Error> + From<<T as transport::AsyncReceive>::Error>,
{
    type Message = ApiRm;
    type Error = RxError;

    async fn receive_with_len(&mut self) -> Result<(Self::Message, usize), RxError> {
        let (msg, msg_len) =
            async_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf).await?;
        Ok((msg.try_into()?, msg_len))
    }
}

/// Send-only channel
#[derive(Debug)]
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
}

impl<T: transport::Transmit, ApiSm, WireSm> AsRef<T> for SenderSimplex<T, ApiSm, WireSm> {
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiSm, WireSm> Sender for SenderSimplex<T, ApiSm, WireSm>
where
    T: transport::Transmit,
    WireSm: Message + From<ApiSm>,
    TxError: From<<T as transport::Transmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    fn send(&mut self, msg: Self::Message) -> Result<(), TxError> {
        sync_impls::send::<T, WireSm>(&mut self.transport, msg.into())
    }
}

impl<T, ApiSm, WireSm> Flushable for SenderSimplex<T, ApiSm, WireSm>
where
    T: transport::Flush,
    WireSm: Message + From<ApiSm>,
    TxError: From<<T as transport::Transmit>::Error>,
{
    fn flush(&mut self) -> Result<(), TxError> {
        Ok(self.transport.flush()?)
    }
}

/// Async variant of [`SenderSimplex`]
#[derive(Debug)]
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
}

impl<T: transport::AsyncTransmit, ApiSm, WireSm> AsRef<T> for AsyncSenderSimplex<T, ApiSm, WireSm> {
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<T, ApiSm, WireSm>
where
    T: transport::AsyncTransmit + Unpin + Send + 'static,
    WireSm: Message + 'static,
    ApiSm: Into<WireSm> + Send + 'static,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        async_impls::send::<_, WireSm>(&mut self.transport, msg.into()).await
    }
}

impl<T, ApiSm, WireSm> AsyncFlushable for AsyncSenderSimplex<T, ApiSm, WireSm>
where
    T: transport::AsyncFlush + Unpin + Send + 'static,
    WireSm: Message + 'static,
    ApiSm: Into<WireSm> + Send + 'static,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
{
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.transport.flush().await?)
    }
}

/// Receive and send channel
#[derive(Debug)]
pub struct Duplex<T: transport::Receive + transport::Transmit, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    receive_buf: Zeroizing<Vec<u8>>,
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
            receive_buf: Zeroizing::default(),
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T: transport::Receive + transport::Transmit, ApiRm, ApiSm, WireRm, WireSm> AsRef<T>
    for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

type DuplexReceiveHalf<T, ApiRm, WireRm> =
    ReceiverSimplex<<T as transport::Splitable>::ReceiveHalf, ApiRm, WireRm>;
type DuplexSendHalf<T, ApiSm, WireSm> =
    SenderSimplex<<T as transport::Splitable>::TransmitHalf, ApiSm, WireSm>;

impl<
    T: transport::Receive + transport::Transmit + transport::Splitable,
    ApiRm,
    ApiSm,
    WireRm,
    WireSm,
> Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
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
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            },
        )
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Transmit,
    WireRm: Message + Default + TryInto<ApiRm>,
    RxError: From<<WireRm as TryInto<ApiRm>>::Error> + From<<T as transport::Receive>::Error>,
{
    type Message = ApiRm;
    type Error = RxError;

    fn receive_with_len(&mut self) -> Result<(ApiRm, usize), RxError> {
        let (msg, msg_len) =
            sync_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf)?;
        Ok((msg.try_into()?, msg_len))
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> TimeoutReceiver for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::TimeoutReceive + transport::Transmit,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
{
    fn receive_with_len_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(Self::Message, usize)>, Self::Error> {
        if let Some((msg, msg_len)) = sync_impls::receive_timeout::<_, WireRm>(
            &mut self.transport,
            &mut self.receive_buf,
            timeout,
        )? {
            Ok(Some((msg.try_into()?, msg_len)))
        } else {
            Ok(None)
        }
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Transmit,
    WireSm: Message + From<ApiSm>,
    TxError: From<<T as transport::Transmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    fn send(&mut self, msg: Self::Message) -> Result<(), TxError> {
        sync_impls::send::<T, WireSm>(&mut self.transport, msg.into())
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Flushable for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Flush,
    WireSm: Message + From<ApiSm>,
    TxError: From<<T as transport::Transmit>::Error>,
{
    fn flush(&mut self) -> Result<(), TxError> {
        Ok(self.transport.flush()?)
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
    receive_buf: Zeroizing<Vec<u8>>,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

/// Send half of a split [`AsyncDuplex`]
pub type AsyncDuplexSendHalf<T, ApiSm, WireSm> =
    AsyncSenderSimplex<<T as transport::AsyncSplitable>::TransmitHalf, ApiSm, WireSm>;
/// Receive half of a split [`AsyncDuplex`]
pub type AsyncDuplexReceiveHalf<T, ApiRm, WireRm> =
    AsyncReceiverSimplex<<T as transport::AsyncSplitable>::ReceiveHalf, ApiRm, WireRm>;

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + transport::AsyncSplitable + Unpin,
{
    /// Create a new [`Self`] with the provided underlying transport
    pub fn new(transport: T) -> Self {
        AsyncDuplex {
            transport,
            receive_buf: Zeroizing::default(),
            _api_rm: PhantomData,
            _api_sm: PhantomData,
            _wire_rm: PhantomData,
            _wire_sm: PhantomData,
        }
    }
}

impl<T: transport::AsyncReceive + transport::AsyncTransmit, ApiRm, ApiSm, WireRm, WireSm> AsRef<T>
    for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + transport::AsyncSplitable + Unpin,
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
    T: transport::AsyncReceive + transport::AsyncTransmit + Send + Unpin + 'static,
    WireRm: Message + Default + Send + 'static,
    ApiRm: TryFrom<WireRm> + Send,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::AsyncReceive>::Error>,
    Self: Send,
{
    type Message = ApiRm;
    type Error = RxError;

    async fn receive_with_len(&mut self) -> Result<(ApiRm, usize), Self::Error> {
        let (msg, msg_len) =
            async_impls::receive::<_, WireRm>(&mut self.transport, &mut self.receive_buf).await?;
        Ok((msg.try_into()?, msg_len))
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + Send + Unpin + 'static,
    WireSm: Message + Send + 'static,
    ApiSm: Into<WireSm> + Send + 'static,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
    Self: Send,
{
    type Message = ApiSm;
    type Error = TxError;

    async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        async_impls::send(&mut self.transport, msg.into()).await
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncFlushable
    for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncFlush + Send + Unpin + 'static,
    WireSm: Message + Send + 'static,
    ApiSm: Into<WireSm> + Send + 'static,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
    Self: Send,
{
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.transport.flush().await?)
    }
}

// Sync
// Control
// Server
pub type ServerControlChannel = Duplex<
    stream::Transceiver,
    control::client_to_server::Message,
    control::server_to_client::Message,
    control::ClientToServer,
    control::ServerToClient,
>;
pub type ServerControlChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;
pub type ServerControlChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
// Client
pub type ClientControlChannel = Duplex<
    stream::Transceiver,
    control::server_to_client::Message,
    control::client_to_server::Message,
    control::ServerToClient,
    control::ClientToServer,
>;
pub type ClientControlChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::ReceiveHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
pub type ClientControlChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;
// Event
// Server
pub type ServerEventChannel = Duplex<
    stream::Transceiver,
    event::client_to_server::Message,
    event::server_to_client::Message,
    event::ClientToServer,
    event::ServerToClient,
>;
pub type ServerEventChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::ReceiveHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
pub type ServerEventChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
// Client
pub type ClientEventChannel = Duplex<
    stream::Transceiver,
    event::server_to_client::Message,
    event::client_to_server::Message,
    event::ServerToClient,
    event::ClientToServer,
>;
pub type ClientEventChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::ReceiveHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
pub type ClientEventChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
// Tunnel
// Server
pub type ServerTunnelChannel =
    Duplex<stream::Transceiver, tunnel::Message, tunnel::Message, tunnel::Message, tunnel::Message>;
pub type ServerTunnelChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::ReceiveHalf,
    tunnel::Message,
    tunnel::Message,
>;
pub type ServerTunnelChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;
// Client
pub type ClientTunnelChannel =
    Duplex<stream::Transceiver, tunnel::Message, tunnel::Message, tunnel::Message, tunnel::Message>;
pub type ClientTunnelChannelReceiveHalf = ReceiverSimplex<
    <stream::Transceiver as transport::Splitable>::ReceiveHalf,
    tunnel::Message,
    tunnel::Message,
>;
pub type ClientTunnelChannelSendHalf = SenderSimplex<
    <stream::Transceiver as transport::Splitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;
// Async
// Control
// Server
pub type AsyncServerControlChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    control::client_to_server::Message,
    control::server_to_client::Message,
    control::ClientToServer,
    control::ServerToClient,
>;
pub type AsyncServerControlChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;
pub type AsyncServerControlChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
// Client
pub type AsyncClientControlChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    control::server_to_client::Message,
    control::client_to_server::Message,
    control::ServerToClient,
    control::ClientToServer,
>;
pub type AsyncClientControlChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    control::server_to_client::Message,
    control::ClientToServer,
>;
pub type AsyncClientControlChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    control::client_to_server::Message,
    control::ClientToServer,
>;
// Event
// Server
pub type AsyncServerEventChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    event::client_to_server::Message,
    event::server_to_client::Message,
    event::ClientToServer,
    event::ServerToClient,
>;
pub type AsyncServerEventChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::ReceiveHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
pub type AsyncServerEventChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
// Client
pub type AsyncClientEventChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    event::server_to_client::Message,
    event::client_to_server::Message,
    event::ServerToClient,
    event::ClientToServer,
>;
pub type AsyncClientEventChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::Splitable>::ReceiveHalf,
    event::server_to_client::Message,
    event::ServerToClient,
>;
pub type AsyncClientEventChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::Splitable>::TransmitHalf,
    event::client_to_server::Message,
    event::ClientToServer,
>;
// Tunnel
// Server
pub type AsyncServerTunnelChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    tunnel::Message,
    tunnel::Message,
    tunnel::Message,
    tunnel::Message,
>;
pub type AsyncServerTunnelChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::Splitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;
pub type AsyncServerTunnelChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::Splitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;
// Client
pub type AsyncClientTunnelChannel = AsyncDuplex<
    stream::AsyncTransceiver,
    tunnel::Message,
    tunnel::Message,
    tunnel::Message,
    tunnel::Message,
>;
pub type AsyncClientTunnelChannelReceiveHalf = AsyncReceiverSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;
pub type AsyncClientTunnelChannelSendHalf = AsyncSenderSimplex<
    <stream::AsyncTransceiver as transport::AsyncSplitable>::TransmitHalf,
    tunnel::Message,
    tunnel::Message,
>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::test_helpers::{EmptyMessage, LargeMessage, MessageWithOptional, SimpleMessage},
        transport::{
            Transmit,
            stream::tests::{
                AsyncInMemReceiver, AsyncInMemTransmitter, InMemReceiver, InMemTransmitter,
                in_mem_async_tx_async_rx, in_mem_async_tx_sync_rx, in_mem_sync_tx_async_rx,
                in_mem_sync_tx_sync_rx,
            },
        },
    };
    use std::{sync::Arc, time::Duration};
    use tokio::runtime::Runtime as TokioRuntime;

    fn build_sync_simplex_pair() -> (
        SenderSimplex<InMemTransmitter, SimpleMessage, SimpleMessage>,
        ReceiverSimplex<InMemReceiver, SimpleMessage, SimpleMessage>,
    ) {
        let (tx, rx) = in_mem_sync_tx_sync_rx();
        (SenderSimplex::new(tx), ReceiverSimplex::new(rx))
    }

    fn build_async_simplex_pair() -> (
        AsyncSenderSimplex<AsyncInMemTransmitter, SimpleMessage, SimpleMessage>,
        AsyncReceiverSimplex<AsyncInMemReceiver, SimpleMessage, SimpleMessage>,
    ) {
        let (tx, rx) = in_mem_async_tx_async_rx();
        (AsyncSenderSimplex::new(tx), AsyncReceiverSimplex::new(rx))
    }

    fn build_large_message_sync_pair() -> (
        SenderSimplex<InMemTransmitter, LargeMessage, LargeMessage>,
        ReceiverSimplex<InMemReceiver, LargeMessage, LargeMessage>,
    ) {
        let (tx, rx) = in_mem_sync_tx_sync_rx();
        (SenderSimplex::new(tx), ReceiverSimplex::new(rx))
    }

    fn build_optional_message_sync_pair() -> (
        SenderSimplex<InMemTransmitter, MessageWithOptional, MessageWithOptional>,
        ReceiverSimplex<InMemReceiver, MessageWithOptional, MessageWithOptional>,
    ) {
        let (tx, rx) = in_mem_sync_tx_sync_rx();
        (SenderSimplex::new(tx), ReceiverSimplex::new(rx))
    }

    fn build_empty_message_sync_pair() -> (
        SenderSimplex<InMemTransmitter, EmptyMessage, EmptyMessage>,
        ReceiverSimplex<InMemReceiver, EmptyMessage, EmptyMessage>,
    ) {
        let (tx, rx) = in_mem_sync_tx_sync_rx();
        (SenderSimplex::new(tx), ReceiverSimplex::new(rx))
    }

    #[test]
    fn test_rx_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RxError>();
    }

    #[test]
    fn test_tx_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxError>();
    }

    #[test]
    fn test_rx_error_from_transport() {
        let transport_err = stream::RxError::Empty;
        let error: RxError = transport_err.into();
        assert!(matches!(error, RxError::Transport(_)));
    }

    #[test]
    fn test_rx_error_display() {
        let error = RxError::Transport(stream::RxError::Empty);
        let display_str = format!("{}", error);
        assert!(display_str.contains("stream is empty and closed"));
    }

    #[test]
    fn test_tx_error_from_transport() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let transport_err = stream::TxError::from(quic_err);
        let error: TxError = transport_err.into();
        assert!(matches!(error, TxError::Transport(_)));
    }

    #[test]
    fn test_error_conversions() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let stream_tx_err = stream::TxError::from(quic_err);
        let codec_tx_err = TxError::from(stream_tx_err);
        assert!(matches!(codec_tx_err, TxError::Transport(_)));

        let stream_rx_err = stream::RxError::Empty;
        let codec_rx_err = RxError::from(stream_rx_err);
        assert!(matches!(codec_rx_err, RxError::Transport(_)));
    }

    #[test]
    fn test_sync_single_message() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "hello world".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_multiple_messages_ordered() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let messages = vec![
            SimpleMessage {
                data: "first".to_string(),
            },
            SimpleMessage {
                data: "second".to_string(),
            },
            SimpleMessage {
                data: "third".to_string(),
            },
            SimpleMessage {
                data: "fourth".to_string(),
            },
        ];

        for msg in &messages {
            sender.send(msg.clone()).expect("send failed");
        }

        for expected in &messages {
            let received = receiver.receive().expect("receive failed");
            assert_eq!(&received, expected);
        }
    }

    #[test]
    fn test_sync_receive_with_len() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "test message".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");
        let (received, byte_len) = receiver.receive_with_len().expect("receive failed");

        assert_eq!(received, msg);
        assert!(byte_len > 0);
        let expected_len = msg.encoded_len() + prost::length_delimiter_len(msg.encoded_len());
        assert_eq!(byte_len, expected_len);
    }

    #[test]
    fn test_sync_empty_message() {
        let (mut sender, mut receiver) = build_empty_message_sync_pair();

        let msg = EmptyMessage {};

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_large_message() {
        let (mut sender, mut receiver) = build_large_message_sync_pair();

        let payload = vec![0xAB; 1024 * 1024];
        let msg = LargeMessage {
            payload: payload.clone(),
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received.payload.len(), payload.len());
        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_very_large_message() {
        let (mut sender, mut receiver) = build_large_message_sync_pair();

        let payload = vec![0xCD; 10 * 1024 * 1024];
        let msg = LargeMessage {
            payload: payload.clone(),
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received.payload.len(), payload.len());
        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_message_with_optional_present() {
        let (mut sender, mut receiver) = build_optional_message_sync_pair();

        let msg = MessageWithOptional {
            required_field: "required".to_string(),
            optional_field: Some("optional".to_string()),
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_message_with_optional_absent() {
        let (mut sender, mut receiver) = build_optional_message_sync_pair();

        let msg = MessageWithOptional {
            required_field: "required".to_string(),
            optional_field: None,
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_rapid_messages() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let count = 1000;
        for i in 0..count {
            let msg = SimpleMessage {
                data: format!("message_{}", i),
            };
            sender.send(msg).expect("send failed");
        }

        for i in 0..count {
            let received = receiver.receive().expect("receive failed");
            assert_eq!(received.data, format!("message_{}", i));
        }
    }

    #[test]
    fn test_sync_interleaved_send_receive() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        for i in 0..100 {
            let msg = SimpleMessage {
                data: format!("msg_{}", i),
            };
            sender.send(msg.clone()).expect("send failed");
            let received = receiver.receive().expect("receive failed");
            assert_eq!(received, msg);
        }
    }

    #[test]
    fn test_sync_timeout_zero_duration() {
        let (_sender, mut receiver) = build_sync_simplex_pair();

        let result = receiver
            .receive_timeout(Duration::ZERO)
            .expect("timeout should not error");

        assert!(result.is_none(), "should timeout immediately");
    }

    #[test]
    fn test_sync_timeout_with_len() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "timeout test".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");

        let result = receiver
            .receive_with_len_timeout(Duration::from_millis(10))
            .expect("receive failed");

        assert!(result.is_some(), "should receive message");
        let (received_msg, byte_len) = result.unwrap();
        assert_eq!(received_msg, msg);
        assert!(byte_len > 0);
    }

    #[test]
    fn test_sync_timeout_with_len_no_data() {
        let (_sender, mut receiver) = build_sync_simplex_pair();

        let result = receiver
            .receive_with_len_timeout(Duration::from_millis(10))
            .expect("timeout should not error");

        assert!(result.is_none(), "should timeout with None");
    }

    #[test]
    fn test_sync_multiple_timeouts() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let result1 = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout 1");
        assert!(result1.is_none());

        sender
            .send(SimpleMessage {
                data: "first".to_string(),
            })
            .expect("send 1");

        let result2 = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout 2");
        assert!(result2.is_some());
        assert_eq!(result2.unwrap().data, "first");

        let result3 = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout 3");
        assert!(result3.is_none());
    }

    #[tokio::test]
    async fn test_async_single_message() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let msg = SimpleMessage {
            data: "async hello".to_string(),
        };

        sender.send(msg.clone()).await.expect("send failed");
        let received = receiver.receive().await.expect("receive failed");

        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn test_async_multiple_messages_ordered() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let messages = vec![
            SimpleMessage {
                data: "async1".to_string(),
            },
            SimpleMessage {
                data: "async2".to_string(),
            },
            SimpleMessage {
                data: "async3".to_string(),
            },
        ];

        for msg in &messages {
            sender.send(msg.clone()).await.expect("send failed");
        }

        for expected in &messages {
            let received = receiver.receive().await.expect("receive failed");
            assert_eq!(&received, expected);
        }
    }

    #[tokio::test]
    async fn test_async_receive_with_len() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let msg = SimpleMessage {
            data: "async len test".to_string(),
        };

        sender.send(msg.clone()).await.expect("send failed");
        let (received, byte_len) = receiver.receive_with_len().await.expect("receive failed");

        assert_eq!(received, msg);
        assert!(byte_len > 0);
    }

    #[tokio::test]
    async fn test_async_rapid_messages() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let count = 500;
        for i in 0..count {
            sender
                .send(SimpleMessage {
                    data: format!("async_{}", i),
                })
                .await
                .expect("send failed");
        }

        for i in 0..count {
            let received = receiver.receive().await.expect("receive failed");
            assert_eq!(received.data, format!("async_{}", i));
        }
    }

    #[tokio::test]
    async fn test_async_concurrent_sends() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let send_task = tokio::spawn(async move {
            for i in 0..100 {
                sender
                    .send(SimpleMessage {
                        data: format!("concurrent_{}", i),
                    })
                    .await
                    .expect("send failed");
            }
            sender
        });

        let mut received_count = 0;
        for _ in 0..100 {
            let _msg = receiver.receive().await.expect("receive failed");
            received_count += 1;
        }

        assert_eq!(received_count, 100);
        let _sender = send_task.await.expect("send task failed");
    }

    #[test]
    fn test_sync_timeout_during_message_receipt() {
        use bytes::BytesMut;
        use prost::Message as ProstMessage;

        let msg = SimpleMessage {
            data: "test partial timeout".to_string(),
        };
        let mut encoded = BytesMut::new();
        msg.encode_length_delimited(&mut encoded).unwrap();
        let full_bytes = encoded.freeze();

        let split_point = full_bytes.len() / 2;
        let first_part = full_bytes.slice(0..split_point);
        let second_part = full_bytes.slice(split_point..);

        let (mut tx, rx) = in_mem_sync_tx_sync_rx();
        let mut receiver = ReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        tx.transmit(first_part).expect("send first part");

        let result = receiver
            .receive_timeout(Duration::from_millis(20))
            .expect("timeout should not error");

        assert!(
            result.is_none(),
            "Should timeout when message is incomplete"
        );

        tx.transmit(second_part).expect("send second part");

        let received = receiver.receive().expect("should receive complete message");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_timeout_during_length_delimiter() {
        use bytes::BytesMut;
        use prost::Message as ProstMessage;

        let msg = SimpleMessage {
            data: "test delimiter timeout".to_string(),
        };

        let mut encoded = BytesMut::new();
        msg.encode_length_delimited(&mut encoded).unwrap();
        let full_bytes = encoded.freeze();

        let (mut tx, rx) = in_mem_sync_tx_sync_rx();
        let mut receiver = ReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        let result = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout should not error");

        assert!(result.is_none(), "Should timeout with no data");

        tx.transmit(full_bytes).expect("send complete");

        let received = receiver.receive().expect("should receive");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_timeout_max_duration() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "delayed message".to_string(),
        };

        let sender_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            sender.send(msg.clone()).expect("send failed");
        });

        let result = receiver
            .receive_timeout(Duration::MAX)
            .expect("receive should not error");

        assert!(
            result.is_some(),
            "should receive with Duration::MAX timeout"
        );
        sender_thread.join().unwrap();
    }

    #[test]
    fn test_sync_timeout_buffer_preservation() {
        use bytes::BytesMut;
        use prost::Message as ProstMessage;

        let msg = SimpleMessage {
            data: "buffer test".to_string(),
        };

        let mut encoded = BytesMut::new();
        msg.encode_length_delimited(&mut encoded).unwrap();
        let full_bytes = encoded.freeze();

        let part1 = full_bytes.slice(0..1);
        let part2 = full_bytes.slice(1..);

        let (mut tx, rx) = in_mem_sync_tx_sync_rx();
        let mut receiver = ReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        tx.transmit(part1).expect("send part1");

        let result = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout should not error");
        assert!(result.is_none(), "should timeout with partial data");

        let result = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout should not error");
        assert!(result.is_none(), "should timeout again with partial data");

        tx.transmit(part2).expect("send part2");

        let received = receiver.receive().expect("should receive complete message");
        assert_eq!(
            received, msg,
            "buffer should preserve partial data across timeouts"
        );
    }

    #[test]
    fn test_sync_to_async() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (tx, rx) = in_mem_sync_tx_async_rx(runtime.clone());

        let mut sender = SenderSimplex::<_, SimpleMessage, SimpleMessage>::new(tx);
        let mut receiver = AsyncReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        let msg = SimpleMessage {
            data: "sync to async".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");

        let received =
            runtime.block_on(async { receiver.receive().await.expect("receive failed") });

        assert_eq!(received, msg);
    }

    #[test]
    fn test_async_to_sync() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (tx, rx) = in_mem_async_tx_sync_rx(runtime.clone());

        let mut sender = AsyncSenderSimplex::<_, SimpleMessage, SimpleMessage>::new(tx);
        let mut receiver = ReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        let msg = SimpleMessage {
            data: "async to sync".to_string(),
        };

        runtime.block_on(async {
            sender.send(msg.clone()).await.expect("send failed");
        });

        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_mixed_timeout() {
        let runtime = Arc::new(TokioRuntime::new().expect("runtime"));
        let (tx, rx) = in_mem_async_tx_sync_rx(runtime.clone());

        let mut sender = AsyncSenderSimplex::<_, SimpleMessage, SimpleMessage>::new(tx);
        let mut receiver = ReceiverSimplex::<_, SimpleMessage, SimpleMessage>::new(rx);

        let result = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("timeout");
        assert!(result.is_none());

        let msg = SimpleMessage {
            data: "mixed".to_string(),
        };
        runtime.block_on(async {
            sender.send(msg.clone()).await.expect("send failed");
        });

        let result = receiver
            .receive_timeout(Duration::from_millis(10))
            .expect("receive failed");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), msg);
    }
}
