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

    pub(super) fn do_receive<T, ApiRm, WireRm>(
        dg_transport: &mut T,
    ) -> Result<(ApiRm, usize), RxError>
    where
        T: transport::Receive,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        RxError: From<<ApiRm as TryFrom<WireRm>>::Error> + From<<T as transport::Receive>::Error>,
    {
        decode(dg_transport.receive()?)
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
        // Test that error types can be converted correctly through the chain
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
        let gate_err = Box::new(io::Error::new(io::ErrorKind::Other, "gate error"))
            as Box<dyn std::error::Error + Send + Sync>;
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
        // Just verify it can be displayed without panicking
        assert!(!display_str.is_empty());
    }
}
