//! Reliable, out-of-order, message-oriented API
//!
//! Built on-top of the [`crate::transport::datagram`] API, this provides methods for sending and
//! receiving messages reliably and out-of-order.

use super::*;
use crate::gate::{self, AsyncGate, Gate};
use crate::transport::{self, datagram};

use aperturec_protocol::media;

use bytes::Bytes;
use prost::Message;
use std::{convert::Infallible, marker::PhantomData};

/// Errors that can occur when receiving messages over an out-of-order channel
#[derive(Debug, thiserror::Error)]
pub enum RxError {
    /// A transport-layer error occurred while receiving data
    #[error(transparent)]
    Transport(#[from] datagram::RxError),

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

/// Errors that can occur when sending messages over an out-of-order channel
#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// A transport-layer error occurred while sending data
    #[error(transparent)]
    Transport(#[from] datagram::TxError),

    /// Failed to encode the message for transmission
    #[error(transparent)]
    Encode(#[from] prost::EncodeError),
}

#[derive(Debug, thiserror::Error)]
pub enum GatedTxError {
    /// A transmission error occurred
    #[error(transparent)]
    Tx(#[from] TxError),

    /// Gate errored while waiting for permission to send
    ///
    /// Gates provide rate limiting. This error occurs when the gate itself encountered an error
    #[error("gate error: {0}")]
    Gate(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub struct Gated<S, G> {
    gate: G,
    ungated: S,
}

impl<S: Sender, G: Gate> Sender for Gated<S, G>
where
    S::Message: Message,
    GatedTxError: From<<S as Sender>::Error>,
{
    type Message = S::Message;
    type Error = GatedTxError;

    fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        let msg_size = msg.encoded_len();
        self.gate.wait(msg_size).map_err(GatedTxError::Gate)?;
        self.ungated.send(msg)?;
        Ok(())
    }
}

impl<S: AsyncSender, G: gate::AsyncGate + Send> AsyncSender for Gated<S, G>
where
    S::Message: Message,
    GatedTxError: From<<S as AsyncSender>::Error>,
{
    type Message = S::Message;
    type Error = GatedTxError;

    async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        let msg_size = msg.encoded_len();
        self.gate.wait(msg_size).await.map_err(GatedTxError::Gate)?;
        self.ungated.send(msg).await?;
        Ok(())
    }
}

impl<S: AsyncFlushable, G: gate::AsyncGate + Send> AsyncFlushable for Gated<S, G>
where
    S::Message: Message,
    GatedTxError: From<<S as AsyncSender>::Error>,
{
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.ungated.flush().await?)
    }
}

fn encode<ApiSm, WireSm>(msg: ApiSm) -> Bytes
where
    WireSm: Message + From<ApiSm>,
{
    Bytes::from(WireSm::from(msg).encode_to_vec())
}

fn decode<ApiRm, WireRm>(dg: Bytes) -> Result<(ApiRm, usize), RxError>
where
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error>,
{
    let dg_len = dg.len();
    let msg = WireRm::decode(dg)?.try_into()?;
    Ok((msg, dg_len))
}

mod sync_impls {
    use super::*;

    pub(super) fn do_send<T, ApiSm, WireSm>(dg_transport: &mut T, msg: ApiSm) -> Result<(), TxError>
    where
        T: transport::Transmit,
        WireSm: Message + From<ApiSm>,
        TxError: From<<T as transport::Transmit>::Error>,
    {
        let dg = encode::<_, WireSm>(msg);
        dg_transport.transmit(dg)?;
        Ok(())
    }

    /// Internal helper for receiving and decoding datagrams with optional timeout.
    fn do_receive_internal<ApiRm, WireRm, E, F>(
        mut recv_fn: F,
    ) -> Result<Option<(ApiRm, usize)>, RxError>
    where
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<E>,
        F: FnMut() -> Result<Option<Bytes>, E>,
    {
        match recv_fn()? {
            Some(bytes) => Ok(Some(decode(bytes)?)),
            None => Ok(None),
        }
    }

    pub(super) fn do_receive<T, ApiRm, WireRm>(
        dg_transport: &mut T,
    ) -> Result<(ApiRm, usize), RxError>
    where
        T: transport::Receive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
    {
        do_receive_internal(|| dg_transport.receive().map(Some))
            .map(|opt| opt.expect("blocking receive should never return None"))
    }

    pub(super) fn do_receive_timeout<T, ApiRm, WireRm>(
        dg_transport: &mut T,
        timeout: Duration,
    ) -> Result<Option<(ApiRm, usize)>, RxError>
    where
        T: transport::TimeoutReceive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
    {
        do_receive_internal(|| dg_transport.receive_timeout(timeout))
    }
}

mod async_impls {
    use super::*;

    pub(super) async fn do_send<T, ApiSm, WireSm>(
        dg_transport: &mut T,
        msg: ApiSm,
    ) -> Result<(), TxError>
    where
        T: transport::AsyncTransmit,
        WireSm: Message + From<ApiSm>,
        TxError: From<<T as transport::AsyncTransmit>::Error>,
    {
        let dg = encode::<_, WireSm>(msg);
        dg_transport.transmit(dg).await?;
        Ok(())
    }

    pub(super) async fn do_receive<T, ApiRm, WireRm>(
        dg_transport: &mut T,
    ) -> Result<(ApiRm, usize), RxError>
    where
        T: transport::AsyncReceive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        RxError:
            From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::AsyncReceive>::Error>,
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
        sync_impls::do_receive(&mut self.transport)
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
        sync_impls::do_receive_timeout(&mut self.transport, timeout)
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

impl<T: transport::AsyncReceive, ApiRm, WireRm> AsRef<T>
    for AsyncReceiverSimplex<T, ApiRm, WireRm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<T, ApiRm, WireRm>
where
    T: transport::AsyncReceive + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::AsyncReceive>::Error>,
{
    type Message = ApiRm;
    type Error = RxError;

    async fn receive_with_len(&mut self) -> Result<(Self::Message, usize), Self::Error> {
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

impl<T: transport::Transmit, ApiSm, WireSm> AsRef<T> for SenderSimplex<T, ApiSm, WireSm> {
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T: transport::Transmit, ApiSm, WireSm> SenderSimplex<T, ApiSm, WireSm>
where
    T: transport::Transmit,
    WireSm: Message,
    ApiSm: TryInto<WireSm>,
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
    WireSm: Message + From<ApiSm>,
    TxError: From<<T as transport::Transmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        sync_impls::do_send::<_, _, WireSm>(&mut self.transport, msg)
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

impl<T: transport::AsyncTransmit, ApiSm, WireSm> AsRef<T> for AsyncSenderSimplex<T, ApiSm, WireSm> {
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<T, ApiSm, WireSm>
where
    T: transport::AsyncTransmit + 'static,
    WireSm: Message + From<ApiSm> + 'static,
    ApiSm: Send,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        async_impls::do_send::<_, _, WireSm>(&mut self.transport, msg).await
    }
}

/// Receive and send channel
#[derive(Debug)]
pub struct Duplex<T: transport::Duplex, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::Duplex, ApiRm, ApiSm, WireRm, WireSm> Duplex<T, ApiRm, ApiSm, WireRm, WireSm> {
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

impl<T: transport::Duplex, ApiRm, ApiSm, WireRm, WireSm> AsRef<T>
    for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::Receive + transport::Transmit,
    WireRm: Message + Default,
    ApiRm: TryFrom<WireRm>,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
{
    type Message = ApiRm;
    type Error = RxError;

    fn receive_with_len(&mut self) -> Result<(Self::Message, usize), Self::Error> {
        sync_impls::do_receive(&mut self.transport)
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
        sync_impls::do_receive_timeout(&mut self.transport, timeout)
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

    fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        sync_impls::do_send::<_, _, WireSm>(&mut self.transport, msg)
    }
}

/// Async variant of [`Duplex`]
#[derive(Debug)]
pub struct AsyncDuplex<T: transport::AsyncDuplex, ApiRm, ApiSm, WireRm, WireSm> {
    transport: T,
    _api_rm: PhantomData<ApiRm>,
    _api_sm: PhantomData<ApiSm>,
    _wire_rm: PhantomData<WireRm>,
    _wire_sm: PhantomData<WireSm>,
}

impl<T: transport::AsyncDuplex, ApiRm, ApiSm, WireRm, WireSm>
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

impl<T: transport::AsyncDuplex, ApiRm, ApiSm, WireRm, WireSm> AsRef<T>
    for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
{
    fn as_ref(&self) -> &T {
        &self.transport
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncReceiver for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + 'static,
    WireRm: Message + Default + 'static,
    ApiRm: TryFrom<WireRm> + Send + 'static,
    RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::AsyncReceive>::Error>,
    Self: Send,
{
    type Message = ApiRm;
    type Error = RxError;

    async fn receive_with_len(&mut self) -> Result<(Self::Message, usize), Self::Error> {
        async_impls::do_receive(&mut self.transport).await
    }
}

impl<T, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<T, ApiRm, ApiSm, WireRm, WireSm>
where
    T: transport::AsyncReceive + transport::AsyncTransmit + 'static,
    ApiSm: Send,
    ApiRm: Send,
    WireRm: Send,
    WireSm: Message + From<ApiSm> + 'static,
    TxError: From<<T as transport::AsyncTransmit>::Error>,
{
    type Message = ApiSm;
    type Error = TxError;

    async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        async_impls::do_send::<_, _, WireSm>(&mut self.transport, msg).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::test_helpers::{EmptyMessage, LargeMessage, MockGate, SimpleMessage},
        transport::datagram::tests::{
            AsyncInMemReceiver, AsyncInMemTransmitter, InMemReceiver, InMemTransmitter,
            in_mem_async_tx_async_rx, in_mem_async_tx_sync_rx, in_mem_sync_tx_async_rx,
            in_mem_sync_tx_sync_rx,
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
    fn test_gated_tx_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GatedTxError>();
    }

    #[test]
    fn test_rx_error_from_transport() {
        let transport_err = datagram::RxError::Accept;
        let error: RxError = transport_err.into();
        assert!(matches!(error, RxError::Transport(_)));
    }

    #[test]
    fn test_tx_error_from_transport() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let transport_err = datagram::TxError::from(quic_err);
        let error: TxError = transport_err.into();
        assert!(matches!(error, TxError::Transport(_)));
    }

    #[test]
    fn test_gated_tx_error_from_tx() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let transport_err = datagram::TxError::from(quic_err);
        let tx_err = TxError::from(transport_err);
        let error: GatedTxError = tx_err.into();
        assert!(matches!(error, GatedTxError::Tx(_)));
    }

    #[test]
    fn test_error_conversions() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let datagram_tx_err = datagram::TxError::from(quic_err);
        let codec_tx_err = TxError::from(datagram_tx_err);
        assert!(matches!(codec_tx_err, TxError::Transport(_)));

        let datagram_rx_err = datagram::RxError::Accept;
        let codec_rx_err = RxError::from(datagram_rx_err);
        assert!(matches!(codec_rx_err, RxError::Transport(_)));
    }

    #[test]
    fn test_gated_tx_error_gate() {
        use std::io;
        let gate_err = Box::new(io::Error::other("gate error"));
        let error = GatedTxError::Gate(gate_err);
        let display_str = format!("{}", error);
        assert!(display_str.contains("gate error"));
    }

    #[test]
    fn test_rx_error_display() {
        let error = RxError::Transport(datagram::RxError::Accept);
        let display_str = format!("{}", error);
        assert!(display_str.contains("connection closed"));
    }

    #[test]
    fn test_tx_error_display() {
        let quic_err =
            crate::quic::Error::from(s2n_quic::connection::Error::immediate_close("test"));
        let transport_err = datagram::TxError::from(quic_err);
        let error = TxError::Transport(transport_err);
        let display_str = format!("{}", error);
        assert!(!display_str.is_empty());
    }

    #[test]
    fn test_sync_single_datagram() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "hello".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_multiple_datagrams() {
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
        ];

        for msg in &messages {
            sender.send(msg.clone()).expect("send failed");
        }

        let mut received = Vec::new();
        for _ in 0..messages.len() {
            received.push(receiver.receive().expect("receive failed"));
        }

        let mut expected_data: Vec<_> = messages.iter().map(|m| &m.data).collect();
        let mut received_data: Vec<_> = received.iter().map(|m| &m.data).collect();
        expected_data.sort();
        received_data.sort();

        assert_eq!(received_data, expected_data);
    }

    #[test]
    fn test_sync_receive_with_len() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "test".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");
        let (received, byte_len) = receiver.receive_with_len().expect("receive failed");

        assert_eq!(received, msg);
        assert!(byte_len > 0);
        assert_eq!(byte_len, msg.encoded_len());
    }

    #[test]
    fn test_sync_empty_datagram() {
        let (mut sender, mut receiver) = build_empty_message_sync_pair();

        let msg = EmptyMessage {};

        sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
    }

    #[test]
    fn test_sync_large_datagram() {
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
    fn test_sync_rapid_datagrams() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let count = 100;
        for i in 0..count {
            sender
                .send(SimpleMessage {
                    data: format!("datagram_{:03}", i),
                })
                .expect("send failed");
        }

        let mut received_data = Vec::new();
        for _ in 0..count {
            let msg = receiver.receive().expect("receive failed");
            received_data.push(msg.data);
        }

        assert_eq!(received_data.len(), count);
        received_data.sort();
        for (i, data) in received_data.iter().enumerate() {
            assert_eq!(data, &format!("datagram_{:03}", i));
        }
    }

    #[test]
    fn test_sync_gated_allows_send() {
        let (sender, mut receiver) = build_sync_simplex_pair();
        let gate = MockGate::new();
        let mut gated_sender = sender.gated(gate.clone());

        let msg = SimpleMessage {
            data: "gated".to_string(),
        };

        gated_sender.send(msg.clone()).expect("send failed");
        let received = receiver.receive().expect("receive failed");

        assert_eq!(received, msg);
        assert_eq!(gate.get_call_count(), 1);
        assert!(gate.get_total_bytes() > 0);
    }

    #[test]
    fn test_sync_gated_blocks_send() {
        let (sender, _receiver) = build_sync_simplex_pair();
        let gate = MockGate::new();
        gate.set_blocking(true);
        let mut gated_sender = sender.gated(gate.clone());

        let msg = SimpleMessage {
            data: "blocked".to_string(),
        };

        let result = gated_sender.send(msg);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GatedTxError::Gate(_)));
        assert_eq!(gate.get_call_count(), 1);
    }

    #[test]
    fn test_sync_gated_byte_count() {
        let (sender, mut receiver) = build_sync_simplex_pair();
        let gate = MockGate::new();
        let mut gated_sender = sender.gated(gate.clone());

        let msg1 = SimpleMessage {
            data: "short".to_string(),
        };
        let msg2 = SimpleMessage {
            data: "much longer message here".to_string(),
        };

        gated_sender.send(msg1.clone()).expect("send 1");
        gated_sender.send(msg2.clone()).expect("send 2");

        let _recv1 = receiver.receive().expect("recv 1");
        let _recv2 = receiver.receive().expect("recv 2");

        assert_eq!(gate.get_call_count(), 2);
        let expected_bytes = msg1.encoded_len() + msg2.encoded_len();
        assert_eq!(gate.get_total_bytes(), expected_bytes);
    }

    #[test]
    fn test_sync_gated_multiple_calls() {
        let (sender, mut receiver) = build_sync_simplex_pair();
        let gate = MockGate::new();
        let mut gated_sender = sender.gated(gate.clone());

        let count = 50;
        for i in 0..count {
            gated_sender
                .send(SimpleMessage {
                    data: format!("msg_{}", i),
                })
                .expect("send failed");
        }

        for _ in 0..count {
            let _msg = receiver.receive().expect("receive failed");
        }

        assert_eq!(gate.get_call_count(), count);
    }

    #[tokio::test]
    async fn test_async_single_datagram() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let msg = SimpleMessage {
            data: "async hello".to_string(),
        };

        sender.send(msg.clone()).await.expect("send failed");
        let received = receiver.receive().await.expect("receive failed");

        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn test_async_multiple_datagrams() {
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

        let mut received = Vec::new();
        for _ in 0..messages.len() {
            received.push(receiver.receive().await.expect("receive failed"));
        }

        let mut expected_data: Vec<_> = messages.iter().map(|m| &m.data).collect();
        let mut received_data: Vec<_> = received.iter().map(|m| &m.data).collect();
        expected_data.sort();
        received_data.sort();

        assert_eq!(received_data, expected_data);
    }

    #[tokio::test]
    async fn test_async_receive_with_len() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let msg = SimpleMessage {
            data: "async len".to_string(),
        };

        sender.send(msg.clone()).await.expect("send failed");
        let (received, byte_len) = receiver.receive_with_len().await.expect("receive failed");

        assert_eq!(received, msg);
        assert_eq!(byte_len, msg.encoded_len());
    }

    #[tokio::test]
    async fn test_async_rapid_datagrams() {
        let (mut sender, mut receiver) = build_async_simplex_pair();

        let count = 200;
        for i in 0..count {
            sender
                .send(SimpleMessage {
                    data: format!("async_{:03}", i),
                })
                .await
                .expect("send failed");
        }

        let mut received_data = Vec::new();
        for _ in 0..count {
            let msg = receiver.receive().await.expect("receive failed");
            received_data.push(msg.data);
        }

        assert_eq!(received_data.len(), count);
        received_data.sort();
        for (i, data) in received_data.iter().enumerate() {
            assert_eq!(data, &format!("async_{:03}", i));
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

        let mut received_data = Vec::new();
        for _ in 0..100 {
            let msg = receiver.receive().await.expect("receive failed");
            received_data.push(msg.data);
        }

        assert_eq!(received_data.len(), 100);
        let _sender = send_task.await.expect("send task");
    }

    #[tokio::test]
    async fn test_async_gated_allows_send() {
        let (sender, mut receiver) = build_async_simplex_pair();
        let gate = MockGate::new();
        let mut gated_sender = sender.gated(gate.clone());

        let msg = SimpleMessage {
            data: "async gated".to_string(),
        };

        gated_sender.send(msg.clone()).await.expect("send failed");
        let received = receiver.receive().await.expect("receive failed");

        assert_eq!(received, msg);
        assert_eq!(gate.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_async_gated_blocks_send() {
        let (sender, _receiver) = build_async_simplex_pair();
        let gate = MockGate::new();
        gate.set_blocking(true);
        let mut gated_sender = sender.gated(gate.clone());

        let msg = SimpleMessage {
            data: "blocked".to_string(),
        };

        let result = gated_sender.send(msg).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GatedTxError::Gate(_)));
    }

    #[tokio::test]
    async fn test_async_gated_multiple_concurrent() {
        let (sender, mut receiver) = build_async_simplex_pair();
        let gate = MockGate::new();
        let mut gated_sender = sender.gated(gate.clone());

        let count = 50;
        let send_task = tokio::spawn(async move {
            for i in 0..count {
                gated_sender
                    .send(SimpleMessage {
                        data: format!("gated_{}", i),
                    })
                    .await
                    .expect("send failed");
            }
            gated_sender
        });

        for _ in 0..count {
            let _msg = receiver.receive().await.expect("receive failed");
        }

        let _sender = send_task.await.expect("send task");
        assert_eq!(gate.get_call_count(), count);
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
    fn test_sync_timeout_with_len() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "len timeout".to_string(),
        };

        sender.send(msg.clone()).expect("send failed");

        let result = receiver
            .receive_with_len_timeout(Duration::from_millis(10))
            .expect("receive failed");

        assert!(result.is_some(), "should receive datagram");
        let (received_msg, byte_len) = result.unwrap();
        assert_eq!(received_msg, msg);
        assert_eq!(byte_len, msg.encoded_len());
    }

    #[test]
    fn test_sync_timeout_with_len_no_datagram() {
        let (_sender, mut receiver) = build_sync_simplex_pair();

        let result = receiver
            .receive_with_len_timeout(Duration::from_millis(10))
            .expect("timeout should not error");

        assert!(result.is_none(), "should timeout with None");
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

    #[test]
    fn test_sync_timeout_max_duration() {
        let (mut sender, mut receiver) = build_sync_simplex_pair();

        let msg = SimpleMessage {
            data: "delayed datagram".to_string(),
        };

        let sender_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            sender.send(msg.clone()).expect("send failed");
            sender
        });

        let result = receiver
            .receive_timeout(Duration::MAX)
            .expect("receive should not error");

        assert!(
            result.is_some(),
            "should receive with Duration::MAX timeout"
        );
        let _sender = sender_thread.join().unwrap();
    }
}
