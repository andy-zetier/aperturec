pub mod der {
    super::impl_codec_reliable!(der, tcp);
    super::impl_codec_unreliable!(der, udp);
    super::tcp_test!(der, 8000);
    super::udp_test!(der, 8500);
}

pub mod cer {
    super::impl_codec_reliable!(cer, tcp);
    super::impl_codec_unreliable!(cer, udp);
    super::tcp_test!(cer, 9000);
    super::udp_test!(der, 9500);
}

pub mod ber {
    super::impl_codec_reliable!(ber, tcp);
    super::impl_codec_unreliable!(der, udp);
    super::tcp_test!(ber, 10000);
    super::udp_test!(der, 10500);
}

macro_rules! impl_codec_reliable {
    ($codec:ident, $transport:ident) => {
        pub mod reliable {
            use crate::reliable::*;
            use crate::*;

            use aperturec_protocol::*;
            use async_trait::async_trait;
            use rasn::de::Needed;
            use rasn::$codec;
            use rasn::{Decode, Encode};
            use std::io::{self, BufRead, Read, Write};
            use std::marker::PhantomData;
            use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};

            const BUF_READER_CAPACITY: usize = 4096 * 32;

            fn do_receive<R: Read, RM: Decode>(
                reader: &mut std::io::BufReader<R>,
                read_bytes: &mut Vec<u8>,
                is_nonblocking: bool,
            ) -> anyhow::Result<RM> {
                let mut nbytes_to_decode = 0;
                loop {
                    while nbytes_to_decode > read_bytes.len() {
                        match reader.fill_buf() {
                            Ok(bytes_filled) => {
                                aperturec_metrics::builtins::rx_bytes(bytes_filled.len());
                                read_bytes.extend(bytes_filled);
                                let nbytes_consumed = bytes_filled.len();
                                reader.consume(nbytes_consumed);
                            }
                            Err(e) => {
                                if is_nonblocking
                                    || (e.kind() != io::ErrorKind::Interrupted
                                        && e.kind() != io::ErrorKind::WouldBlock)
                                {
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                    match $codec::decode(&read_bytes[..nbytes_to_decode]) {
                        Err(rasn::ber::de::Error::Incomplete {
                            needed: Needed::Unknown,
                        })
                        | Err(rasn::ber::de::Error::FieldError { .. }) => {
                            nbytes_to_decode += 1;
                        }
                        Err(rasn::ber::de::Error::Incomplete {
                            needed: Needed::Size(size),
                        }) => {
                            nbytes_to_decode += size.get();
                        }
                        Ok(msg) => {
                            read_bytes.drain(..nbytes_to_decode);
                            return Ok(msg);
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
            }

            fn do_send<W: Write + NonblockableIO, SM: Encode>(
                writer: &mut W,
                msg: SM,
                write_bytes: &mut Vec<u8>,
                is_nonblocking: bool,
            ) -> anyhow::Result<()> {
                write_bytes.extend($codec::encode(&msg)?);

                while !write_bytes.is_empty() {
                    match writer.write(&write_bytes) {
                        Ok(nbytes_written) => {
                            aperturec_metrics::builtins::tx_bytes(nbytes_written);
                            write_bytes.drain(..nbytes_written);
                        }
                        Err(e) => {
                            if (!is_nonblocking
                                && (e.kind() != io::ErrorKind::WouldBlock
                                    || e.kind() != io::ErrorKind::Interrupted))
                                || is_nonblocking
                            {
                                return Err(e.into());
                            }
                        }
                    }
                }
                writer.flush()?;
                Ok(())
            }

            async fn do_receive_async<R: AsyncRead + Unpin, RM: Decode>(
                reader: &mut tokio::io::BufReader<R>,
                read_bytes: &mut Vec<u8>,
            ) -> anyhow::Result<RM> {
                let mut nbytes_to_decode = 0;
                loop {
                    while nbytes_to_decode > read_bytes.len() {
                        let bytes_filled = reader.fill_buf().await?;
                        aperturec_metrics::builtins::rx_bytes(bytes_filled.len());
                        read_bytes.extend(bytes_filled);
                        let nbytes_consumed = bytes_filled.len();
                        reader.consume(nbytes_consumed);
                    }

                    match $codec::decode(&read_bytes[..nbytes_to_decode]) {
                        Err(rasn::ber::de::Error::Incomplete {
                            needed: Needed::Unknown,
                        })
                        | Err(rasn::ber::de::Error::FieldError { .. }) => {
                            nbytes_to_decode += 1;
                        }
                        Err(rasn::ber::de::Error::Incomplete {
                            needed: Needed::Size(size),
                        }) => {
                            nbytes_to_decode += size.get();
                        }
                        Ok(msg) => {
                            read_bytes.drain(..nbytes_to_decode);
                            return Ok(msg);
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
            }

            async fn do_send_async<W: AsyncWrite + Unpin, SM: Encode>(
                writer: &mut W,
                msg: SM,
            ) -> anyhow::Result<()> {
                let mut buf = $codec::encode(&msg)?;
                let total_write_size = buf.len();
                let mut buf_slice: &[u8] = &mut buf;
                let mut nbytes_written = 0;
                loop {
                    nbytes_written += writer.write_buf(&mut buf_slice).await?;
                    aperturec_metrics::builtins::tx_bytes(nbytes_written);
                    if nbytes_written >= total_write_size {
                        break;
                    }
                }
                writer.flush().await?;
                Ok(())
            }

            pub struct ReceiverSimplex<R, RM>
            where
                R: Read,
                RM: Decode,
            {
                reader: std::io::BufReader<R>,
                read_bytes: Vec<u8>,
                _receive_message: PhantomData<RM>,
            }

            impl<R, RM> ReceiverSimplex<R, RM>
            where
                R: Read,
                RM: Decode,
            {
                pub fn new(reader: R) -> Self {
                    ReceiverSimplex {
                        reader: std::io::BufReader::with_capacity(BUF_READER_CAPACITY, reader),
                        read_bytes: vec![],
                        _receive_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> R {
                    self.reader.into_inner()
                }
            }

            impl<R, RM> AsRef<R> for ReceiverSimplex<R, RM>
            where
                R: Read,
                RM: Decode,
            {
                fn as_ref(&self) -> &R {
                    self.reader.get_ref()
                }
            }

            impl<R, RM> AsMut<R> for ReceiverSimplex<R, RM>
            where
                R: Read,
                RM: Decode,
            {
                fn as_mut(&mut self) -> &mut R {
                    self.reader.get_mut()
                }
            }

            impl<R, RM> Receiver for ReceiverSimplex<R, RM>
            where
                R: Read + NonblockableIO,
                RM: Decode,
            {
                type Message = RM;

                fn receive(&mut self) -> anyhow::Result<Self::Message> {
                    let is_nonblocking = self.reader.get_ref().is_nonblocking();
                    do_receive(&mut self.reader, &mut self.read_bytes, is_nonblocking)
                }
            }

            pub struct AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRead,
                RM: Decode,
            {
                reader: tokio::io::BufReader<R>,
                read_bytes: Vec<u8>,
                _receive_message: PhantomData<RM>,
            }

            impl<R, RM> AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRead + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                pub fn new(reader: R) -> Self {
                    AsyncReceiverSimplex {
                        reader: tokio::io::BufReader::with_capacity(BUF_READER_CAPACITY, reader),
                        read_bytes: vec![],
                        _receive_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> R {
                    self.reader.into_inner()
                }
            }

            impl<R, RM> AsRef<R> for AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRead + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                fn as_ref(&self) -> &R {
                    self.reader.get_ref()
                }
            }

            impl<R, RM> AsMut<R> for AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRead + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                fn as_mut(&mut self) -> &mut R {
                    self.reader.get_mut()
                }
            }

            #[async_trait]
            impl<R, RM> AsyncReceiver for AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRead + Send + Sync + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                type Message = RM;

                async fn receive(&mut self) -> anyhow::Result<Self::Message> {
                    do_receive_async(&mut self.reader, &mut self.read_bytes).await
                }
            }

            pub struct SenderSimplex<W, SM>
            where
                W: Write + NonblockableIO,
                SM: Encode,
            {
                writer: W,
                write_bytes: Vec<u8>,
                _send_message: PhantomData<SM>,
            }

            impl<W, SM> SenderSimplex<W, SM>
            where
                W: Write + NonblockableIO,
                SM: Encode,
            {
                pub fn new(writer: W) -> Self {
                    SenderSimplex {
                        writer,
                        write_bytes: vec![],
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> W {
                    self.writer
                }
            }

            impl<W, SM> AsRef<W> for SenderSimplex<W, SM>
            where
                W: Write + NonblockableIO,
                SM: Encode,
            {
                fn as_ref(&self) -> &W {
                    &self.writer
                }
            }

            impl<W, SM> AsMut<W> for SenderSimplex<W, SM>
            where
                W: Write + NonblockableIO,
                SM: Encode,
            {
                fn as_mut(&mut self) -> &mut W {
                    &mut self.writer
                }
            }

            impl<W, SM> Sender for SenderSimplex<W, SM>
            where
                W: Write + NonblockableIO,
                SM: Encode,
            {
                type Message = SM;

                fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    let is_nonblocking = self.writer.is_nonblocking();
                    do_send(&mut self.writer, msg, &mut self.write_bytes, is_nonblocking)
                }
            }

            pub struct AsyncSenderSimplex<W, SM>
            where
                W: AsyncWrite + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                writer: W,
                _send_message: PhantomData<SM>,
            }

            impl<W, SM> AsyncSenderSimplex<W, SM>
            where
                W: AsyncWrite + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                pub fn new(writer: W) -> Self {
                    AsyncSenderSimplex {
                        writer,
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> W {
                    self.writer
                }
            }

            impl<W, SM> AsRef<W> for AsyncSenderSimplex<W, SM>
            where
                W: AsyncWrite + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                fn as_ref(&self) -> &W {
                    &self.writer
                }
            }

            impl<W, SM> AsMut<W> for AsyncSenderSimplex<W, SM>
            where
                W: AsyncWrite + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                fn as_mut(&mut self) -> &mut W {
                    &mut self.writer
                }
            }

            #[async_trait]
            impl<W, SM> AsyncSender for AsyncSenderSimplex<W, SM>
            where
                W: AsyncWrite + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                type Message = SM;

                async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    do_send_async(&mut self.writer, msg).await
                }
            }

            pub struct Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                common_rw: std::io::BufReader<C>,
                read_bytes: Vec<u8>,
                write_bytes: Vec<u8>,
                _receive_message: PhantomData<RM>,
                _send_message: PhantomData<SM>,
            }

            impl<C, RM, SM> Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                pub fn new(common_rw: C) -> Self {
                    Duplex {
                        common_rw: std::io::BufReader::with_capacity(
                            BUF_READER_CAPACITY,
                            common_rw,
                        ),
                        read_bytes: vec![],
                        write_bytes: vec![],
                        _receive_message: PhantomData,
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> C {
                    self.common_rw.into_inner()
                }
            }

            impl<C, RM, SM> Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO + Clone,
                RM: Decode,
                SM: Encode,
            {
                pub fn split(self) -> (ReceiverSimplex<C, RM>, SenderSimplex<C, SM>) {
                    let wh = self.common_rw.get_ref().clone();

                    (
                        ReceiverSimplex {
                            reader: self.common_rw,
                            read_bytes: self.read_bytes,
                            _receive_message: PhantomData,
                        },
                        SenderSimplex {
                            writer: wh,
                            write_bytes: self.write_bytes,
                            _send_message: PhantomData,
                        },
                    )
                }
            }

            impl<C, RM, SM> AsRef<C> for Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                fn as_ref(&self) -> &C {
                    self.common_rw.get_ref()
                }
            }

            impl<C, RM, SM> AsMut<C> for Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                fn as_mut(&mut self) -> &mut C {
                    self.common_rw.get_mut()
                }
            }

            impl<C, RM, SM> Receiver for Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                type Message = RM;
                fn receive(&mut self) -> anyhow::Result<RM> {
                    let is_nonblocking = self.common_rw.get_ref().is_nonblocking();
                    do_receive(&mut self.common_rw, &mut self.read_bytes, is_nonblocking)
                }
            }

            impl<C, RM, SM> Sender for Duplex<C, RM, SM>
            where
                C: Read + Write + NonblockableIO,
                RM: Decode,
                SM: Encode,
            {
                type Message = SM;
                fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    let is_nonblocking = self.common_rw.get_ref().is_nonblocking();
                    do_send(
                        self.common_rw.get_mut(),
                        msg,
                        &mut self.write_bytes,
                        is_nonblocking,
                    )
                }
            }

            pub struct AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                common_rw: tokio::io::BufReader<C>,
                read_bytes: Vec<u8>,
                _receive_message: PhantomData<RM>,
                _send_message: PhantomData<SM>,
            }

            impl<C, RM, SM> AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                pub fn new(common_rw: C) -> Self {
                    AsyncDuplex {
                        common_rw: tokio::io::BufReader::with_capacity(
                            BUF_READER_CAPACITY,
                            common_rw,
                        ),
                        read_bytes: vec![],
                        _receive_message: PhantomData,
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> C {
                    self.common_rw.into_inner()
                }

                pub fn split(
                    self,
                ) -> (
                    AsyncSenderSimplex<tokio::io::WriteHalf<C>, SM>,
                    AsyncReceiverSimplex<tokio::io::ReadHalf<C>, RM>,
                ) {
                    let (rh, wh) = tokio::io::split(self.common_rw.into_inner());
                    (AsyncSenderSimplex::new(wh), AsyncReceiverSimplex::new(rh))
                }

                pub fn unsplit(
                    tx: AsyncSenderSimplex<tokio::io::WriteHalf<C>, SM>,
                    rx: AsyncReceiverSimplex<tokio::io::ReadHalf<C>, RM>,
                ) -> Self {
                    let common_rw = rx.into_inner().unsplit(tx.into_inner());
                    AsyncDuplex::new(common_rw)
                }
            }

            impl<C, RM, SM> AsRef<C> for AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                fn as_ref(&self) -> &C {
                    self.common_rw.get_ref()
                }
            }

            impl<C, RM, SM> AsMut<C> for AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                fn as_mut(&mut self) -> &mut C {
                    self.common_rw.get_mut()
                }
            }

            #[async_trait]
            impl<C, RM, SM> AsyncReceiver for AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                type Message = RM;
                async fn receive(&mut self) -> anyhow::Result<RM> {
                    do_receive_async(&mut self.common_rw, &mut self.read_bytes).await
                }
            }

            #[async_trait]
            impl<C, RM, SM> AsyncSender for AsyncDuplex<C, RM, SM>
            where
                C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
                SM: Encode + Send + 'static,
            {
                type Message = SM;
                async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    do_send_async(&mut self.common_rw, msg).await
                }
            }

            pub type ServerControlChannel = Duplex<
                $transport::Server<$transport::Accepted>,
                control_messages::ClientToServerMessage,
                control_messages::ServerToClientMessage,
            >;

            pub type ClientControlChannel = Duplex<
                $transport::Client<$transport::Connected>,
                control_messages::ServerToClientMessage,
                control_messages::ClientToServerMessage,
            >;

            pub type ServerEventChannel = ReceiverSimplex<
                $transport::Server<$transport::Accepted>,
                event_messages::ClientToServerMessage,
            >;

            pub type ClientEventChannel = SenderSimplex<
                $transport::Client<$transport::Connected>,
                event_messages::ClientToServerMessage,
            >;

            pub type AsyncServerControlChannel = AsyncDuplex<
                $transport::Server<$transport::AsyncAccepted>,
                control_messages::ClientToServerMessage,
                control_messages::ServerToClientMessage,
            >;

            pub type AsyncClientControlChannel = AsyncDuplex<
                $transport::Client<$transport::AsyncConnected>,
                control_messages::ServerToClientMessage,
                control_messages::ClientToServerMessage,
            >;

            pub type AsyncServerEventChannel = AsyncReceiverSimplex<
                $transport::Server<$transport::AsyncAccepted>,
                event_messages::ClientToServerMessage,
            >;

            pub type AsyncClientEventChannel = AsyncSenderSimplex<
                $transport::Client<$transport::AsyncConnected>,
                event_messages::ClientToServerMessage,
            >;
        }
    };
}
pub(crate) use impl_codec_reliable;

macro_rules! impl_codec_unreliable {
    ($codec:ident, $transport:ident) => {
        pub mod unreliable {
            use crate::unreliable::*;
            use crate::*;

            use aperturec_protocol::*;
            use async_trait::async_trait;
            use rasn::$codec;
            use rasn::{Decode, Encode};
            use std::marker::PhantomData;

            pub struct ReceiverSimplex<R, RM>
            where
                R: RawReceiver,
                RM: Decode,
            {
                receiver: R,
                buf: [u8; 65536],
                _receive_message: PhantomData<RM>,
            }

            impl<R, RM> ReceiverSimplex<R, RM>
            where
                R: RawReceiver,
                RM: Decode,
            {
                pub fn new(raw: R) -> Self {
                    ReceiverSimplex {
                        receiver: raw,
                        buf: [0_u8; 65536],
                        _receive_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> R {
                    self.receiver
                }
            }

            impl<R, RM> Receiver for ReceiverSimplex<R, RM>
            where
                R: RawReceiver,
                RM: Decode,
            {
                type Message = RM;
                fn receive(&mut self) -> anyhow::Result<Self::Message> {
                    let nbytes_recvd = self.receiver.receive(&mut self.buf)?;
                    aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
                    let slice = &self.buf[0..nbytes_recvd];
                    Ok($codec::decode::<RM>(&slice)?)
                }
            }

            pub struct AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRawReceiver + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                receiver: R,
                buf: [u8; 65536],
                _receive_message: PhantomData<RM>,
            }

            impl<R, RM> AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRawReceiver + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                pub fn new(raw: R) -> Self {
                    AsyncReceiverSimplex {
                        receiver: raw,
                        buf: [0_u8; 65536],
                        _receive_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> R {
                    self.receiver
                }
            }

            #[async_trait]
            impl<R, RM> AsyncReceiver for AsyncReceiverSimplex<R, RM>
            where
                R: AsyncRawReceiver + Send + Unpin + 'static,
                RM: Decode + Send + 'static,
            {
                type Message = RM;

                async fn receive(&mut self) -> anyhow::Result<Self::Message> {
                    let nbytes_recvd = self.receiver.receive(&mut self.buf).await?;
                    aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
                    let slice = &self.buf[0..nbytes_recvd];
                    Ok($codec::decode::<RM>(&slice)?)
                }
            }

            pub struct SenderSimplex<S, SM>
            where
                S: RawSender,
                SM: Encode,
            {
                sender: S,
                _send_message: PhantomData<SM>,
            }

            impl<S, SM> SenderSimplex<S, SM>
            where
                S: RawSender,
                SM: Encode,
            {
                pub fn new(raw: S) -> Self {
                    SenderSimplex {
                        sender: raw,
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> S {
                    self.sender
                }
            }

            impl<S, SM> Sender for SenderSimplex<S, SM>
            where
                S: RawSender,
                SM: Encode,
            {
                type Message = SM;
                fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    let buf = $codec::encode(&msg)?;
                    let nbytes_sent = self.sender.send(&buf)?;
                    aperturec_metrics::builtins::tx_bytes(nbytes_sent);
                    Ok(())
                }
            }

            pub struct AsyncSenderSimplex<S, SM>
            where
                S: AsyncRawSender + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                sender: S,
                _send_message: PhantomData<SM>,
            }

            impl<S, SM> AsyncSenderSimplex<S, SM>
            where
                S: AsyncRawSender + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                pub fn new(raw: S) -> Self {
                    AsyncSenderSimplex {
                        sender: raw,
                        _send_message: PhantomData,
                    }
                }

                pub fn into_inner(self) -> S {
                    self.sender
                }
            }

            #[async_trait]
            impl<S, SM> AsyncSender for AsyncSenderSimplex<S, SM>
            where
                S: AsyncRawSender + Send + Unpin + 'static,
                SM: Encode + Send + 'static,
            {
                type Message = SM;

                async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                    let buf = $codec::encode(&msg)?;
                    let nbytes_sent = self.sender.send(&buf).await?;
                    aperturec_metrics::builtins::tx_bytes(nbytes_sent);
                    Ok(())
                }
            }

            pub type ServerMediaChannel = SenderSimplex<
                $transport::Client<$transport::Connected>,
                media_messages::ServerToClientMessage,
            >;

            pub type ClientMediaChannel = ReceiverSimplex<
                $transport::Server<$transport::Listening>,
                media_messages::ServerToClientMessage,
            >;

            pub type AsyncServerMediaChannel = AsyncSenderSimplex<
                $transport::Client<$transport::AsyncConnected>,
                media_messages::ServerToClientMessage,
            >;

            pub type AsyncClientMediaChannel = AsyncReceiverSimplex<
                $transport::Server<$transport::AsyncListening>,
                media_messages::ServerToClientMessage,
            >;
        }
    };
}
pub(crate) use impl_codec_unreliable;

macro_rules! tcp_test {
    ($codec:ident, $port_start:literal) => {
        #[cfg(test)]
        mod tcp_test {
            use super::reliable::*;
            use crate::reliable::*;
            use crate::{AsyncReceiver, AsyncSender, Receiver, Sender};

            use aperturec_protocol::common_types::*;
            use aperturec_protocol::control_messages;
            use aperturec_protocol::event_messages;
            use aperturec_state_machine::TryTransitionable;
            use std::net::SocketAddr;

            macro_rules! tcp_client_and_server {
                ($ip:expr, $port:expr) => {{
                    let tcp_server = tcp::Server::new(SocketAddr::from(($ip, $port)))
                        .try_transition()
                        .await
                        .expect("failed to listen");
                    let tcp_client = tcp::Client::new(SocketAddr::from(($ip, $port)))
                        .try_transition()
                        .await
                        .expect("Failed to connect");
                    let tcp_server = tcp_server.try_transition().await.expect("Failed to accept");
                    (tcp_client, tcp_server)
                }};
            }

            macro_rules! async_tcp_client_and_server {
                ($ip:expr, $port:expr) => {{
                    let (sync_client, sync_server) = tcp_client_and_server!($ip, $port);
                    let async_client = sync_client
                        .try_transition()
                        .await
                        .expect("failed to make client async");
                    let async_server = sync_server
                        .try_transition()
                        .await
                        .expect("failed to make server async");
                    (async_client, async_server)
                }};
            }

            macro_rules! client_init {
                ($client_id:literal) => {{
                    control_messages::ClientToServerMessage::new_client_init(
                        control_messages::ClientInitBuilder::default()
                            .temp_id(ClientId($client_id))
                            .client_info(
                                control_messages::ClientInfoBuilder::default()
                                    .version(
                                        SemVerBuilder::default()
                                            .major(0)
                                            .minor(1)
                                            .patch(2)
                                            .build()
                                            .expect("SemVer build"),
                                    )
                                    .build_id("asdf".into())
                                    .os(control_messages::Os::new_linux())
                                    .os_version("Bionic Beaver".into())
                                    .ssl_library("OpenSSL".into())
                                    .ssl_version("1.2".into())
                                    .bitness(control_messages::Bitness::new_b64())
                                    .endianness(control_messages::Endianness::new_big())
                                    .architecture(control_messages::Architecture::new_x86())
                                    .cpu_id("Haswell".into())
                                    .number_of_cores(4)
                                    .amount_of_ram("2.4Gb".into())
                                    .display_size(
                                        DimensionBuilder::default()
                                            .height(1024)
                                            .width(768)
                                            .build()
                                            .expect("Dimension build"),
                                    )
                                    .build()
                                    .expect("ClientInfo build"),
                            )
                            .client_caps(
                                ClientCapsBuilder::default()
                                    .supported_codecs(vec![Codec::new_zlib()])
                                    .build()
                                    .expect("ClientCaps build"),
                            )
                            .client_heartbeat_interval(DurationMs(1000))
                            .client_heartbeat_response_interval(DurationMs(1000))
                            .decoders(vec![control_messages::DecoderBuilder::default()
                                .port(9999)
                                .build()
                                .expect("Decoder build")])
                            .build()
                            .expect("ClientInit build"),
                    )
                }};
            }

            macro_rules! server_init {
                () => {{
                    control_messages::ServerToClientMessage::new_server_init(
                        control_messages::ServerInitBuilder::default()
                            .client_id(ClientId(1))
                            .server_name("Some sweet server".into())
                            .event_port(12345)
                            .display_size(
                                DimensionBuilder::default()
                                    .width(800)
                                    .height(600)
                                    .build()
                                    .expect("ServerInit build"),
                            )
                            .cursor_bitmaps(Some(vec![
                                CursorBitmap {
                                    cursor: Cursor::new_default(),
                                    data: vec![110, 20, 30].into(),
                                },
                                CursorBitmap {
                                    cursor: Cursor::new_text(),
                                    data: vec![11, 22, 33].into(),
                                },
                            ]))
                            .decoder_areas(vec![].into())
                            .build()
                            .expect("ServerInit build"),
                    )
                }};
            }

            #[tokio::test]
            async fn cc_client_init() {
                let (tcp_client, tcp_server) = tcp_client_and_server!([127, 0, 0, 1], $port_start);
                let mut server_cc = ServerControlChannel::new(tcp_server);
                let mut client_cc = ClientControlChannel::new(tcp_client);

                let ci = client_init!(1234);
                client_cc
                    .send(ci.clone())
                    .expect("Failed to send ClientInit");
                let received_ci = server_cc.receive().expect("Failed to receive ClientInit");

                assert_eq!(received_ci, ci);
                let _tcp_server = server_cc.into_inner();
                let _tcp_client = client_cc.into_inner();
            }

            #[tokio::test]
            async fn cc_client_init_async() {
                let (tcp_client, tcp_server) =
                    async_tcp_client_and_server!([127, 0, 0, 1], $port_start + 1);
                let mut server_cc = AsyncServerControlChannel::new(tcp_server);
                let mut client_cc = AsyncClientControlChannel::new(tcp_client);

                let ci = client_init!(1234);
                client_cc
                    .send(ci.clone())
                    .await
                    .expect("Failed to send ClientInit");
                let received_ci = server_cc
                    .receive()
                    .await
                    .expect("Failed to receive ClientInit");

                assert_eq!(received_ci, ci);
                let _tcp_server = server_cc.into_inner();
                let _tcp_client = client_cc.into_inner();
            }

            #[tokio::test]
            async fn cc_server_init() {
                let (tcp_client, tcp_server) =
                    tcp_client_and_server!([127, 0, 0, 1], $port_start + 2);
                let mut server_cc = ServerControlChannel::new(tcp_server);
                let mut client_cc = ClientControlChannel::new(tcp_client);

                let si = server_init!();
                server_cc
                    .send(si.clone())
                    .expect("Failed to send ClientInit");
                let received_si = client_cc.receive().expect("Failed to receive ServerInit");

                assert_eq!(received_si, si);
                let _tcp_server = server_cc.into_inner();
                let _tcp_client = client_cc.into_inner();
            }

            #[tokio::test]
            async fn cc_server_init_async() {
                let (tcp_client, tcp_server) =
                    async_tcp_client_and_server!([127, 0, 0, 1], $port_start + 3);
                let mut server_cc = AsyncServerControlChannel::new(tcp_server);
                let mut client_cc = AsyncClientControlChannel::new(tcp_client);

                let si = server_init!();
                server_cc
                    .send(si.clone())
                    .await
                    .expect("Failed to send ClientInit");
                let received_si = client_cc
                    .receive()
                    .await
                    .expect("Failed to receive ServerInit");

                assert_eq!(received_si, si);
                let _tcp_server = server_cc.into_inner();
                let _tcp_client = client_cc.into_inner();
            }

            #[tokio::test]
            async fn ec_init() {
                let (tcp_client, tcp_server) =
                    tcp_client_and_server!([127, 0, 0, 1], $port_start + 4);
                let mut server_ec = ServerEventChannel::new(tcp_server);
                let mut client_ec = ClientEventChannel::new(tcp_client);

                let msg = event_messages::ClientToServerMessage::new_key_event(
                    event_messages::KeyEventBuilder::default()
                        .down(true)
                        .key(1)
                        .build()
                        .expect("KeyEvent build"),
                );
                client_ec.send(msg.clone()).expect("event channel send");
                let rx = server_ec.receive().expect("event channel receive");

                assert_eq!(msg, rx);
            }

            #[tokio::test]
            async fn ec_init_async() {
                let (tcp_client, tcp_server) =
                    async_tcp_client_and_server!([127, 0, 0, 1], $port_start + 5);
                let mut server_ec = AsyncServerEventChannel::new(tcp_server);
                let mut client_ec = AsyncClientEventChannel::new(tcp_client);

                let msg = event_messages::ClientToServerMessage::new_key_event(
                    event_messages::KeyEventBuilder::default()
                        .down(true)
                        .key(1)
                        .build()
                        .expect("KeyEvent build"),
                );
                client_ec
                    .send(msg.clone())
                    .await
                    .expect("event channel send");
                let rx = server_ec.receive().await.expect("event channel receive");

                assert_eq!(msg, rx);
            }
        }
    };
}
pub(crate) use tcp_test;

macro_rules! udp_test {
    ($codec:ident, $port_start:literal) => {
        #[cfg(test)]
        mod udp_test {
            use super::unreliable::*;
            use crate::unreliable::*;
            use crate::{AsyncReceiver, AsyncSender, Receiver, Sender};

            use aperturec_protocol::common_types::*;
            use aperturec_protocol::media_messages::*;
            use aperturec_state_machine::TryTransitionable;
            use std::net::SocketAddr;

            macro_rules! udp_client_and_server {
                ($ip:expr, $port:expr) => {{
                    let udp_server = udp::Server::new(SocketAddr::from(($ip, $port)))
                        .try_transition()
                        .await
                        .expect("Failed to listen");
                    let udp_client = udp::Client::new(SocketAddr::from(($ip, $port)))
                        .try_transition()
                        .await
                        .expect("Failed to connect");
                    (udp_client, udp_server)
                }};
            }

            macro_rules! async_udp_client_and_server {
                ($ip:expr, $port:expr) => {{
                    let (sync_client, sync_server) = udp_client_and_server!($ip, $port);
                    let async_client = sync_client
                        .try_transition()
                        .await
                        .expect("Failed to make client async");
                    let async_server = sync_server
                        .try_transition()
                        .await
                        .expect("Failed to make server async");
                    (async_client, async_server)
                }};
            }

            macro_rules! test_media_message {
                () => {{
                    ServerToClientMessage::new_framebuffer_update(FramebufferUpdate::new(vec![
                        RectangleUpdateBuilder::default()
                            .sequence_id(SequenceId(1))
                            .location(Location::new(0, 0))
                            .rectangle(Rectangle::new(
                                Codec::new_raw(),
                                vec![0xc5; 1024].into(),
                                None,
                            ))
                            .build()
                            .expect("RectangleUpdate build"),
                    ]))
                }};
            }

            #[tokio::test]
            async fn mc_init() {
                let (sync_client, sync_server) =
                    udp_client_and_server!([127, 0, 0, 1], $port_start);
                let mut server_mc = ServerMediaChannel::new(sync_client);
                let mut client_mc = ClientMediaChannel::new(sync_server);
                let msg = test_media_message!();
                server_mc
                    .send(msg.clone())
                    .expect("failed to send media message");
                let recvd = client_mc
                    .receive()
                    .expect("failed to receive media message");
                assert_eq!(msg, recvd);

                let _sync_client = server_mc.into_inner();
                let _sync_server = client_mc.into_inner();
            }

            #[tokio::test]
            async fn mc_init_async() {
                let (async_client, async_server) =
                    async_udp_client_and_server!([127, 0, 0, 1], $port_start + 1);
                let mut server_mc = AsyncServerMediaChannel::new(async_client);
                let mut client_mc = AsyncClientMediaChannel::new(async_server);
                let msg = test_media_message!();
                server_mc
                    .send(msg.clone())
                    .await
                    .expect("failed to send media message");
                let recvd = client_mc
                    .receive()
                    .await
                    .expect("failed to receive media message");
                assert_eq!(msg, recvd);

                let _sync_client = server_mc.into_inner();
                let _sync_server = client_mc.into_inner();
            }
        }
    };
}
pub(crate) use udp_test;
