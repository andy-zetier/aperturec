//! Byte-oriented, reliable, in-order transport
use crate::util::Syncify;

use std::io::{self, Read, Write};
use std::marker::Unpin;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::runtime::Runtime as TokioRuntime;

/// A trait for stream transports which can be split into receive ([`Read`]) and transmit
/// ([`Write`]) halves
pub trait Splitable: Read + Write {
    /// The receive half type, which must be [`Read`]
    type ReceiveHalf: Read;
    /// The transmit half type, which must be [`Write`]
    type TransmitHalf: Write;

    /// Split the transport into it's two parts
    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf);
}

/// Async variant of [`Splitable`]
///
/// Composite parts must implement [`AsyncRead`] and [`AsyncWrite`] respectively
pub trait AsyncSplitable: AsyncRead + AsyncWrite {
    type ReceiveHalf: AsyncRead;
    type TransmitHalf: AsyncWrite;

    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf);
}

mod sync_impls {
    use super::*;

    pub fn read<R: AsyncRead + Unpin>(
        mut reader: R,
        rt: &TokioRuntime,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        reader.read(buf).syncify(rt)
    }

    pub fn write<W: AsyncWrite + Unpin>(
        mut writer: W,
        rt: &TokioRuntime,
        buf: &[u8],
    ) -> io::Result<usize> {
        writer.write(buf).syncify(rt)
    }

    pub fn flush<W: AsyncWrite + Unpin>(mut writer: W, rt: &TokioRuntime) -> io::Result<()> {
        writer.flush().syncify(rt)
    }
}

mod async_impls {
    use super::*;

    pub fn poll_read<R: AsyncRead + Unpin>(
        reader: R,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        pin!(reader).poll_read(cx, buf)
    }

    pub fn poll_write<W: AsyncWrite + Unpin>(
        writer: W,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        pin!(writer).poll_write(cx, buf)
    }

    pub fn poll_flush<W: AsyncWrite + Unpin>(
        writer: W,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        pin!(writer).poll_flush(cx)
    }

    pub fn poll_shutdown<W: AsyncWrite + Unpin>(
        writer: W,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        pin!(writer).poll_shutdown(cx)
    }
}

/// A byte-oriented sender and receiver
#[derive(Debug)]
pub struct Transceiver {
    stream: s2n_quic::stream::BidirectionalStream,
    async_rt: Arc<TokioRuntime>,
}

impl Transceiver {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::BidirectionalStream, async_rt: Arc<TokioRuntime>) -> Self {
        Transceiver { stream, async_rt }
    }
}

impl Read for Transceiver {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        sync_impls::read(&mut self.stream, &self.async_rt, buf)
    }
}

impl Write for Transceiver {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        sync_impls::write(&mut self.stream, &self.async_rt, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        sync_impls::flush(&mut self.stream, &self.async_rt)
    }
}

impl Splitable for Transceiver {
    type ReceiveHalf = Receiver;
    type TransmitHalf = Transmitter;

    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
        let (rh, wh) = self.stream.split();
        (
            Receiver {
                stream: rh,
                async_rt: self.async_rt.clone(),
            },
            Transmitter {
                stream: wh,
                async_rt: self.async_rt,
            },
        )
    }
}

/// A byte-oriented sender
#[derive(Debug)]
pub struct Transmitter {
    stream: s2n_quic::stream::SendStream,
    async_rt: Arc<TokioRuntime>,
}

impl Transmitter {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::SendStream, async_rt: Arc<TokioRuntime>) -> Self {
        Transmitter { stream, async_rt }
    }
}

impl Write for Transmitter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        sync_impls::write(&mut self.stream, &self.async_rt, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        sync_impls::flush(&mut self.stream, &self.async_rt)
    }
}

/// A byte-oriented receiver
#[derive(Debug)]
pub struct Receiver {
    stream: s2n_quic::stream::ReceiveStream,
    async_rt: Arc<TokioRuntime>,
}

impl Receiver {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::ReceiveStream, async_rt: Arc<TokioRuntime>) -> Self {
        Receiver { stream, async_rt }
    }
}

impl Read for Receiver {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        sync_impls::read(&mut self.stream, &self.async_rt, buf)
    }
}

/// Async variant of [`Transceiver`]
#[derive(Debug)]
pub struct AsyncTransceiver {
    stream: s2n_quic::stream::BidirectionalStream,
}

impl AsyncTransceiver {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::BidirectionalStream) -> Self {
        AsyncTransceiver { stream }
    }
}

impl AsyncRead for AsyncTransceiver {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        async_impls::poll_read(&mut self.stream, cx, buf)
    }
}

impl AsyncWrite for AsyncTransceiver {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        async_impls::poll_write(&mut self.stream, cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        async_impls::poll_flush(&mut self.stream, cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        async_impls::poll_shutdown(&mut self.stream, cx)
    }
}

impl AsyncSplitable for AsyncTransceiver {
    type ReceiveHalf = AsyncReceiver;
    type TransmitHalf = AsyncTransmitter;

    fn split(self) -> (Self::ReceiveHalf, Self::TransmitHalf) {
        let (rh, wh) = self.stream.split();
        (
            AsyncReceiver { stream: rh },
            AsyncTransmitter { stream: wh },
        )
    }
}

/// Async variant of [`Transmitter`]
#[derive(Debug)]
pub struct AsyncTransmitter {
    stream: s2n_quic::stream::SendStream,
}

impl AsyncTransmitter {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::SendStream) -> Self {
        AsyncTransmitter { stream }
    }
}

impl AsyncWrite for AsyncTransmitter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        async_impls::poll_write(&mut self.stream, cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        async_impls::poll_flush(&mut self.stream, cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        async_impls::poll_shutdown(&mut self.stream, cx)
    }
}

/// Async variant of [`Receiver`]
#[derive(Debug)]
pub struct AsyncReceiver {
    stream: s2n_quic::stream::ReceiveStream,
}

impl AsyncReceiver {
    /// Create a new [`Self`]
    pub fn new(stream: s2n_quic::stream::ReceiveStream) -> Self {
        AsyncReceiver { stream }
    }
}

impl AsyncRead for AsyncReceiver {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        async_impls::poll_read(&mut self.stream, cx, buf)
    }
}
