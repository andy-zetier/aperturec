pub mod der {
    super::impl_codec_reliable!(der, tcp, rasn::der::de::Error::Incomplete { .. });
    super::tcp_test!(der, 8000);
}

pub mod cer {
    super::impl_codec_reliable!(
        cer,
        tcp,
        rasn::ber::de::Error::Incomplete { .. },
        rasn::ber::de::Error::FieldError { .. }
    );
    super::tcp_test!(cer, 9000);
}

pub mod ber {
    super::impl_codec_reliable!(ber, tcp, rasn::ber::de::Error::Incomplete { .. });
    super::tcp_test!(ber, 10000);
}

macro_rules! impl_codec_reliable {
    ($codec:ident, $transport:ident, $( $incomplete_error:pat ),*) => {
        pub mod reliable {
        use crate::reliable::*;

        use aperturec_protocol::*;
        use async_trait::async_trait;
        use rasn::$codec;
        use rasn::{Decode, Encode};
        use std::io::{Read, Write};
        use std::marker::PhantomData;
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

        fn do_receive<R: Read, RM: Decode>(reader: &mut R) -> anyhow::Result<RM> {
            let mut buf: Vec<u8> = vec![];

            loop {
                match $codec::decode::<RM>(&buf) {
                    $(
                        Err($incomplete_error) => {
                            let mut byte = [0_u8; 1];
                            reader.read(&mut byte)?;
                            buf.push(byte[0]);
                        }
                    )*,
                    Ok(msg) => break Ok(msg),
                    Err(e) => {
                        break Err(e.into());
                    }
                }
            }
        }

        fn do_send<W: Write, SM: Encode>(writer: &mut W, msg: SM) -> anyhow::Result<()> {
            let buf = $codec::encode(&msg)?;
            writer.write(&buf)?;
            Ok(())
        }

        async fn do_receive_async<R: AsyncRead + Unpin, RM: Decode>(
            reader: &mut R,
        ) -> anyhow::Result<RM> {
            let mut buf: Vec<u8> = vec![];

            loop {
                match $codec::decode::<RM>(&buf) {
                    $(
                        Err($incomplete_error) => {
                            let mut byte = [0_u8; 1];
                            reader.read(&mut byte).await?;
                            buf.push(byte[0]);
                        }
                    )*,
                    Ok(msg) => break Ok(msg),
                    Err(e) => break Err(e.into()),
                }
            }
        }

        async fn do_send_async<W: AsyncWrite + Unpin, SM: Encode>(
            writer: &mut W,
            msg: SM,
        ) -> anyhow::Result<()> {
            let buf = $codec::encode(&msg)?;
            writer.write(&buf).await?;
            Ok(())
        }

        pub struct ReceiverSimplex<R, RM>
        where
            R: Read,
            RM: Decode,
        {
            reader: R,
            _receive_message: PhantomData<RM>,
        }

        impl<R, RM> ReceiverSimplex<R, RM>
        where
            R: Read,
            RM: Decode,
        {
            pub fn new(reader: R) -> Self {
                ReceiverSimplex {
                    reader: reader,
                    _receive_message: PhantomData,
                }
            }

            pub fn into_inner(self) -> R {
                self.reader
            }
        }

        impl<R, RM> Receiver for ReceiverSimplex<R, RM>
        where
            R: Read,
            RM: Decode,
        {
            type Message = RM;

            fn receive(&mut self) -> anyhow::Result<Self::Message> {
                do_receive(&mut self.reader)
            }
        }

        pub struct AsyncReceiverSimplex<R, RM>
        where
            R: AsyncRead,
            RM: Decode,
        {
            reader: R,
            _receive_message: PhantomData<RM>,
        }

        impl<R, RM> AsyncReceiverSimplex<R, RM>
        where
            R: AsyncRead + Send + Unpin + 'static,
            RM: Decode + Send + 'static,
        {
            pub fn new(reader: R) -> Self {
                AsyncReceiverSimplex {
                    reader: reader,
                    _receive_message: PhantomData,
                }
            }

            pub fn into_inner(self) -> R {
                self.reader
            }
        }

        #[async_trait]
        impl<R, RM> AsyncReceiver for AsyncReceiverSimplex<R, RM>
        where
            R: AsyncRead + Send + Unpin + 'static,
            RM: Decode + Send + 'static,
        {
            type Message = RM;

            async fn receive(&mut self) -> anyhow::Result<Self::Message> {
                do_receive_async(&mut self.reader).await
            }
        }

        pub struct SenderSimplex<W, SM>
        where
            W: Write,
            SM: Encode,
        {
            writer: W,
            _send_message: PhantomData<SM>,
        }

        impl<W, SM> SenderSimplex<W, SM>
        where
            W: Write,
            SM: Encode,
        {
            pub fn new(writer: W) -> Self {
                SenderSimplex {
                    writer,
                    _send_message: PhantomData,
                }
            }

            pub fn into_inner(self) -> W {
                self.writer
            }
        }

        impl<W, SM> Sender for SenderSimplex<W, SM>
        where
            W: Write,
            SM: Encode,
        {
            type Message = SM;

            fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                do_send(&mut self.writer, msg)
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
            C: Read + Write,
            RM: Decode,
            SM: Encode,
        {
            common_rw: C,
            _receive_message: PhantomData<RM>,
            _send_message: PhantomData<SM>,
        }

        impl<C, RM, SM> Duplex<C, RM, SM>
        where
            C: Read + Write,
            RM: Decode,
            SM: Encode,
        {
            pub fn new(common_rw: C) -> Self {
                Duplex {
                    common_rw,
                    _receive_message: PhantomData,
                    _send_message: PhantomData,
                }
            }

            pub fn into_inner(self) -> C {
                self.common_rw
            }
        }

        impl<C, RM, SM> Receiver for Duplex<C, RM, SM>
        where
            C: Read + Write,
            RM: Decode,
            SM: Encode,
        {
            type Message = RM;
            fn receive(&mut self) -> anyhow::Result<RM> {
                do_receive(&mut self.common_rw)
            }
        }

        impl<C, RM, SM> Sender for Duplex<C, RM, SM>
        where
            C: Read + Write,
            RM: Decode,
            SM: Encode,
        {
            type Message = SM;
            fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
                do_send(&mut self.common_rw, msg)
            }
        }

        pub struct AsyncDuplex<C, RM, SM>
        where
            C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
            RM: Decode + Send + 'static,
            SM: Encode + Send + 'static,
        {
            common_rw: C,
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
                    common_rw,
                    _receive_message: PhantomData,
                    _send_message: PhantomData,
                }
            }

            pub fn into_inner(self) -> C {
                self.common_rw
            }
        }

        #[async_trait]
        impl<C, RM, SM> AsyncReceiver for AsyncDuplex<C, RM, SM>
        where
            C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
            RM: Decode + Send + 'static,
            SM: Encode + Send + 'static,
        {
            type Message = RM;
            async fn receive(&mut self) -> anyhow::Result<RM> {
                do_receive_async(&mut self.common_rw).await
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

macro_rules! tcp_test {
    ($codec:ident, $port_start:literal) => {
        #[cfg(test)]
        mod test {
            use super::reliable::*;
            use crate::reliable::*;

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
                                    .supported_codecs(vec![Codec::new_avif()])
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
