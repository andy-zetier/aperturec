pub mod reliable {
    use crate::reliable::*;
    use crate::*;

    use aperturec_protocol::*;
    use prost::Message;
    use std::error::Error;
    use std::io::{ErrorKind, Read, Write};
    use std::marker::PhantomData;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    fn do_receive<R: Read, RM: Message + Default>(
        reader: &mut std::io::BufReader<R>,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<(RM, usize)> {
        if buf.is_empty() {
            buf.resize(1, 0);
            reader.read_exact(&mut buf[0..])?;
            aperturec_metrics::builtins::rx_bytes(1);
        }

        let msg_len = loop {
            match prost::decode_length_delimiter(&**buf) {
                Ok(delim) => break delim,
                Err(_) => {
                    let cur_len = buf.len();
                    buf.resize(cur_len + 1, 0);
                    reader.read_exact(&mut buf[cur_len..])?;
                    aperturec_metrics::builtins::rx_bytes(1);
                }
            }
        };

        let delim_len = prost::length_delimiter_len(msg_len);
        let total_len = delim_len + msg_len;
        buf.resize(total_len, 0);
        reader.read_exact(&mut buf[delim_len..])?;
        aperturec_metrics::builtins::rx_bytes(msg_len);

        let msg = match RM::decode(&buf[delim_len..]) {
            Ok(msg) => msg,
            Err(e) => return Err(e.into()),
        };

        buf.clear();
        Ok((msg, msg_len))
    }

    fn do_send<W: Write, SM: Message>(
        writer: &mut W,
        msg: SM,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let msg_len = msg.encoded_len();
        let delim_len = prost::length_delimiter_len(msg_len);
        let nbytes = msg_len + delim_len;

        if buf.len() < nbytes {
            buf.resize(nbytes, 0);
        }
        msg.encode_length_delimited(&mut &mut buf[..])?;

        writer.write_all(&buf[..nbytes])?;
        writer.flush()?;
        aperturec_metrics::builtins::tx_bytes(nbytes);
        Ok(())
    }

    async fn read_and_extend<R: AsyncRead + Unpin>(
        nbytes: usize,
        reader: &mut tokio::io::BufReader<R>,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let mut nbytes_read = 0;
        let mut b = [0_u8; 1];
        while nbytes_read < nbytes {
            nbytes_read += match reader.read(&mut b).await {
                Ok(nbytes) => {
                    aperturec_metrics::builtins::rx_bytes(nbytes);
                    nbytes
                }
                Err(e) => {
                    if e.kind() != ErrorKind::Interrupted {
                        return Err(e.into());
                    } else {
                        0
                    }
                }
            };
            buf.push(b[0]);
        }
        Ok(())
    }

    async fn do_receive_async<R: AsyncRead + Unpin, RM: Message + Default>(
        reader: &mut tokio::io::BufReader<R>,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<(RM, usize)> {
        if buf.is_empty() {
            read_and_extend(1, reader, buf).await?;
        }

        let msg_len = loop {
            match prost::decode_length_delimiter(&**buf) {
                Ok(delim) => break delim,
                Err(_) => read_and_extend(1, reader, buf).await?,
            }
        };

        let delim_len = prost::length_delimiter_len(msg_len);
        read_and_extend(msg_len, reader, buf).await?;

        let msg = match RM::decode(&buf[delim_len..]) {
            Ok(msg) => msg,
            Err(e) => return Err(e.into()),
        };

        buf.clear();
        Ok((msg, msg_len))
    }

    async fn do_send_async<W: AsyncWrite + Unpin, SM: Message>(
        writer: &mut W,
        msg: SM,
        buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let msg_len = msg.encoded_len();
        let delim_len = prost::length_delimiter_len(msg_len);
        let nbytes = msg_len + delim_len;

        if buf.len() < nbytes {
            buf.resize(nbytes, 0);
        }
        msg.encode_length_delimited(&mut &mut buf[..])?;

        writer.write_all(&buf[..nbytes]).await?;
        writer.flush().await?;
        aperturec_metrics::builtins::tx_bytes(nbytes);
        Ok(())
    }

    pub struct ReceiverSimplex<R: Read, ApiRm, WireRm> {
        reader: std::io::BufReader<R>,
        receive_buf: Vec<u8>,
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: Read, ApiRm, WireRm> ReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(reader: R) -> Self {
            ReceiverSimplex {
                reader: std::io::BufReader::new(reader),
                receive_buf: vec![],
                _api_rm: PhantomData,
                _wire_rm: PhantomData,
            }
        }

        pub fn into_inner(self) -> R {
            self.reader.into_inner()
        }
    }

    impl<R: Read, ApiRm, WireRm> AsRef<R> for ReceiverSimplex<R, ApiRm, WireRm> {
        fn as_ref(&self) -> &R {
            self.reader.get_ref()
        }
    }

    impl<R: Read, ApiRm, WireRm> AsMut<R> for ReceiverSimplex<R, ApiRm, WireRm> {
        fn as_mut(&mut self) -> &mut R {
            self.reader.get_mut()
        }
    }

    impl<R, ApiRm, WireRm> Receiver for ReceiverSimplex<R, ApiRm, WireRm>
    where
        R: Read,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiRm;

        fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let (msg, len) = do_receive::<R, WireRm>(&mut self.reader, &mut self.receive_buf)?;
            Ok((msg.try_into()?, len))
        }
    }

    pub struct AsyncReceiverSimplex<R: AsyncRead, ApiRm, WireRm> {
        reader: tokio::io::BufReader<R>,
        receive_buf: Vec<u8>,
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: AsyncRead, ApiRm, WireRm> AsyncReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(reader: R) -> Self {
            AsyncReceiverSimplex {
                reader: tokio::io::BufReader::new(reader),
                receive_buf: vec![],
                _api_rm: PhantomData,
                _wire_rm: PhantomData,
            }
        }

        pub fn into_inner(self) -> R {
            self.reader.into_inner()
        }
    }

    impl<R: AsyncRead, ApiRm, WireRm> AsRef<R> for AsyncReceiverSimplex<R, ApiRm, WireRm> {
        fn as_ref(&self) -> &R {
            self.reader.get_ref()
        }
    }

    impl<R: AsyncRead, ApiRm, WireRm> AsMut<R> for AsyncReceiverSimplex<R, ApiRm, WireRm> {
        fn as_mut(&mut self) -> &mut R {
            self.reader.get_mut()
        }
    }

    impl<R, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<R, ApiRm, WireRm>
    where
        R: AsyncRead + Unpin + Send + 'static,
        WireRm: Message + Default + 'static,
        ApiRm: TryFrom<WireRm> + Send + 'static,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiRm;

        async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let (msg, len) =
                do_receive_async::<R, WireRm>(&mut self.reader, &mut self.receive_buf).await?;
            Ok((msg.try_into()?, len))
        }
    }

    pub struct SenderSimplex<W: Write, ApiSm, WireSm> {
        writer: W,
        send_buf: Vec<u8>,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<W: Write, ApiSm, WireSm> SenderSimplex<W, ApiSm, WireSm> {
        pub fn new(writer: W) -> Self {
            SenderSimplex {
                writer,
                send_buf: vec![],
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> W {
            self.writer
        }
    }

    impl<W: Write, ApiSm, WireSm> AsRef<W> for SenderSimplex<W, ApiSm, WireSm> {
        fn as_ref(&self) -> &W {
            &self.writer
        }
    }

    impl<W: Write, ApiSm, WireSm> AsMut<W> for SenderSimplex<W, ApiSm, WireSm> {
        fn as_mut(&mut self) -> &mut W {
            &mut self.writer
        }
    }

    impl<W, ApiSm, WireSm> Sender for SenderSimplex<W, ApiSm, WireSm>
    where
        W: Write,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiSm;

        fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            do_send::<W, WireSm>(&mut self.writer, msg.try_into()?, &mut self.send_buf)
        }
    }

    pub struct AsyncSenderSimplex<W: AsyncWrite, ApiSm, WireSm> {
        writer: W,
        send_buf: Vec<u8>,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<W: AsyncWrite, ApiSm, WireSm> AsyncSenderSimplex<W, ApiSm, WireSm> {
        pub fn new(writer: W) -> Self {
            AsyncSenderSimplex {
                writer,
                send_buf: vec![],
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> W {
            self.writer
        }
    }

    impl<W: AsyncWrite, ApiSm, WireSm> AsRef<W> for AsyncSenderSimplex<W, ApiSm, WireSm> {
        fn as_ref(&self) -> &W {
            &self.writer
        }
    }

    impl<W: AsyncWrite, ApiSm, WireSm> AsMut<W> for AsyncSenderSimplex<W, ApiSm, WireSm> {
        fn as_mut(&mut self) -> &mut W {
            &mut self.writer
        }
    }

    impl<W, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<W, ApiSm, WireSm>
    where
        W: AsyncWrite + Unpin + Send + 'static,
        WireSm: Message + 'static,
        ApiSm: TryInto<WireSm> + Send + 'static,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
    {
        type Message = ApiSm;

        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            do_send_async::<W, WireSm>(&mut self.writer, msg.try_into()?, &mut self.send_buf).await
        }
    }

    pub struct Duplex<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> {
        common_rw: std::io::BufReader<C>,
        receive_buf: Vec<u8>,
        send_buf: Vec<u8>,
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> Duplex<C, ApiRm, ApiSm, WireRm, WireSm> {
        pub fn new(common_rw: C) -> Self {
            Duplex {
                common_rw: std::io::BufReader::new(common_rw),
                receive_buf: vec![],
                send_buf: vec![],
                _api_rm: PhantomData,
                _api_sm: PhantomData,
                _wire_rm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> C {
            self.common_rw.into_inner()
        }
    }

    impl<C: Read + Write + Clone, ApiRm, ApiSm, WireRm, WireSm>
        Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        pub fn split(
            self,
        ) -> (
            ReceiverSimplex<C, ApiRm, WireRm>,
            SenderSimplex<C, ApiSm, WireSm>,
        ) {
            let wh = self.common_rw.get_ref().clone();

            (
                ReceiverSimplex {
                    reader: self.common_rw,
                    receive_buf: self.receive_buf,
                    _api_rm: PhantomData,
                    _wire_rm: PhantomData,
                },
                SenderSimplex {
                    writer: wh,
                    send_buf: self.send_buf,
                    _api_sm: PhantomData,
                    _wire_sm: PhantomData,
                },
            )
        }
    }

    impl<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> AsRef<C>
        for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        fn as_ref(&self) -> &C {
            self.common_rw.get_ref()
        }
    }

    impl<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> AsMut<C>
        for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        fn as_mut(&mut self) -> &mut C {
            self.common_rw.get_mut()
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: Read + Write,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Send + Sync + Error + 'static,
    {
        type Message = ApiRm;
        fn receive_with_len(&mut self) -> anyhow::Result<(ApiRm, usize)> {
            let (msg, len) = do_receive::<C, WireRm>(&mut self.common_rw, &mut self.receive_buf)?;
            Ok((msg.try_into()?, len))
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: Read + Write,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Send + Sync + Error + 'static,
    {
        type Message = ApiSm;
        fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            do_send::<C, WireSm>(
                self.common_rw.get_mut(),
                msg.try_into()?,
                &mut self.send_buf,
            )
        }
    }

    pub struct AsyncDuplex<C: AsyncRead + AsyncWrite, ApiRm, ApiSm, WireRm, WireSm> {
        common_rw: tokio::io::BufReader<C>,
        receive_buf: Vec<u8>,
        send_buf: Vec<u8>,
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    pub type AsyncDuplexSendHalf<C, ApiSm, WireSm> =
        AsyncSenderSimplex<tokio::io::WriteHalf<C>, ApiSm, WireSm>;
    pub type AsyncDuplexReceiveHalf<C, ApiRm, WireRm> =
        AsyncReceiverSimplex<tokio::io::ReadHalf<C>, ApiRm, WireRm>;

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        pub fn new(common_rw: C) -> Self {
            AsyncDuplex {
                common_rw: tokio::io::BufReader::new(common_rw),
                receive_buf: vec![],
                send_buf: vec![],
                _api_rm: PhantomData,
                _api_sm: PhantomData,
                _wire_rm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> C {
            self.common_rw.into_inner()
        }

        pub fn split(
            self,
        ) -> (
            AsyncDuplexSendHalf<C, ApiSm, WireSm>,
            AsyncDuplexReceiveHalf<C, ApiRm, WireRm>,
        ) {
            let (rh, wh) = tokio::io::split(self.common_rw.into_inner());
            (AsyncSenderSimplex::new(wh), AsyncReceiverSimplex::new(rh))
        }

        pub fn unsplit(
            tx: AsyncDuplexSendHalf<C, ApiSm, WireSm>,
            rx: AsyncDuplexReceiveHalf<C, ApiRm, WireRm>,
        ) -> Self {
            let common_rw = rx.into_inner().unsplit(tx.into_inner());
            AsyncDuplex::new(common_rw)
        }
    }

    impl<C: AsyncRead + AsyncWrite, ApiRm, ApiSm, WireRm, WireSm> AsRef<C>
        for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        fn as_ref(&self) -> &C {
            self.common_rw.get_ref()
        }
    }

    impl<C: AsyncRead + AsyncWrite, ApiRm, ApiSm, WireRm, WireSm> AsMut<C>
        for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        fn as_mut(&mut self) -> &mut C {
            self.common_rw.get_mut()
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncReceiver for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        WireRm: Message + Default + Send + 'static,
        ApiRm: TryFrom<WireRm> + Send + 'static,
        <ApiRm as TryFrom<WireRm>>::Error: Send + Sync + Error + 'static,
        ApiSm: Send + 'static,
        WireSm: Send + 'static,
    {
        type Message = ApiRm;

        async fn receive_with_len(&mut self) -> anyhow::Result<(ApiRm, usize)> {
            let (msg, len) =
                do_receive_async::<C, WireRm>(&mut self.common_rw, &mut self.receive_buf).await?;
            Ok((msg.try_into()?, len))
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        WireSm: Message + Send + 'static,
        ApiSm: TryInto<WireSm> + Send + 'static,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
        ApiRm: TryFrom<WireRm> + Send + 'static,
        WireRm: Message + Send + 'static,
    {
        type Message = ApiSm;
        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            do_send_async(&mut self.common_rw, msg.try_into()?, &mut self.send_buf).await
        }
    }

    pub type ServerControlChannel = Duplex<
        tcp::Server<tcp::Accepted>,
        control::client_to_server::Message,
        control::server_to_client::Message,
        control::ClientToServer,
        control::ServerToClient,
    >;

    pub type ClientControlChannel = Duplex<
        tcp::Client<tcp::Connected>,
        control::server_to_client::Message,
        control::client_to_server::Message,
        control::ServerToClient,
        control::ClientToServer,
    >;

    pub type ServerEventChannel = ReceiverSimplex<
        tcp::Server<tcp::Accepted>,
        event::client_to_server::Message,
        event::ClientToServer,
    >;

    pub type ClientEventChannel = SenderSimplex<
        tcp::Client<tcp::Connected>,
        event::client_to_server::Message,
        event::ClientToServer,
    >;

    pub type AsyncServerControlChannel = AsyncDuplex<
        tcp::Server<tcp::AsyncAccepted>,
        control::client_to_server::Message,
        control::server_to_client::Message,
        control::ClientToServer,
        control::ServerToClient,
    >;
    pub type AsyncServerControlChannelReadHalf = AsyncReceiverSimplex<
        tokio::io::ReadHalf<tcp::Server<tcp::AsyncAccepted>>,
        control::client_to_server::Message,
        control::ClientToServer,
    >;
    pub type AsyncServerControlChannelWriteHalf = AsyncSenderSimplex<
        tokio::io::WriteHalf<tcp::Server<tcp::AsyncAccepted>>,
        control::server_to_client::Message,
        control::ServerToClient,
    >;

    pub type AsyncClientControlChannel = AsyncDuplex<
        tcp::Client<tcp::AsyncConnected>,
        control::server_to_client::Message,
        control::client_to_server::Message,
        control::ServerToClient,
        control::ClientToServer,
    >;
    pub type AsyncClientControlChannelReadHalf = AsyncReceiverSimplex<
        tokio::io::ReadHalf<tcp::Client<tcp::AsyncConnected>>,
        control::server_to_client::Message,
        control::ServerToClient,
    >;
    pub type AsyncClientControlChannelWriteHalf = AsyncSenderSimplex<
        tokio::io::WriteHalf<tcp::Client<tcp::AsyncConnected>>,
        control::client_to_server::Message,
        control::ClientToServer,
    >;

    pub type AsyncServerEventChannel = AsyncReceiverSimplex<
        tcp::Server<tcp::AsyncAccepted>,
        event::client_to_server::Message,
        event::ClientToServer,
    >;

    pub type AsyncClientEventChannel = AsyncSenderSimplex<
        tcp::Client<tcp::AsyncConnected>,
        event::client_to_server::Message,
        event::ClientToServer,
    >;
}

pub mod unreliable {
    use crate::unreliable::*;
    use crate::*;

    use aperturec_protocol::*;
    use prost::Message;
    use std::error::Error;
    use std::marker::PhantomData;

    pub struct ReceiverSimplex<R: RawReceiver, ApiRm, WireRm> {
        receiver: R,
        buf: [u8; 65536],
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: RawReceiver, ApiRm, WireRm> ReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(raw: R) -> Self {
            ReceiverSimplex {
                receiver: raw,
                buf: [0_u8; 65536],
                _api_rm: PhantomData,
                _wire_rm: PhantomData,
            }
        }

        pub fn into_inner(self) -> R {
            self.receiver
        }
    }

    impl<R, ApiRm, WireRm> Receiver for ReceiverSimplex<R, ApiRm, WireRm>
    where
        R: RawReceiver,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiRm;

        fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let nbytes_recvd = self.receiver.receive(&mut self.buf)?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok((WireRm::decode(slice)?.try_into()?, nbytes_recvd))
        }
    }

    pub struct AsyncReceiverSimplex<R: AsyncRawReceiver, ApiRm, WireRm> {
        receiver: R,
        buf: [u8; 65536],
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: AsyncRawReceiver, ApiRm, WireRm> AsyncReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(raw: R) -> Self {
            AsyncReceiverSimplex {
                receiver: raw,
                buf: [0_u8; 65536],
                _api_rm: PhantomData,
                _wire_rm: PhantomData,
            }
        }

        pub fn into_inner(self) -> R {
            self.receiver
        }
    }

    impl<R, ApiRm, WireRm> AsyncReceiver for AsyncReceiverSimplex<R, ApiRm, WireRm>
    where
        R: AsyncRawReceiver + 'static,
        WireRm: Message + Default + 'static,
        ApiRm: TryFrom<WireRm> + Send + 'static,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
    {
        type Message = ApiRm;

        async fn receive(&mut self) -> anyhow::Result<Self::Message> {
            let (msg, _) = self.receive_with_len().await?;
            Ok(msg)
        }

        async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let nbytes_recvd = self.receiver.receive(&mut self.buf).await?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok((WireRm::decode(slice)?.try_into()?, nbytes_recvd))
        }
    }

    pub struct SenderSimplex<S: RawSender, ApiSm, WireSm> {
        sender: S,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<S: RawSender, ApiSm, WireSm> SenderSimplex<S, ApiSm, WireSm> {
        pub fn new(raw: S) -> Self {
            SenderSimplex {
                sender: raw,
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> S {
            self.sender
        }
    }

    impl<S, ApiSm, WireSm> Sender for SenderSimplex<S, ApiSm, WireSm>
    where
        S: RawSender,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiSm;
        fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            let buf = msg.try_into()?.encode_to_vec();
            let nbytes_sent = self.sender.send(&buf)?;
            aperturec_metrics::builtins::tx_bytes(nbytes_sent);
            Ok(())
        }
    }

    pub struct AsyncSenderSimplex<S: AsyncRawSender, ApiSm, WireSm> {
        sender: S,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<S: AsyncRawSender, ApiSm, WireSm> AsyncSenderSimplex<S, ApiSm, WireSm> {
        pub fn new(raw: S) -> Self {
            AsyncSenderSimplex {
                sender: raw,
                _api_sm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> S {
            self.sender
        }
    }

    impl<S, ApiSm, WireSm> AsyncSender for AsyncSenderSimplex<S, ApiSm, WireSm>
    where
        S: AsyncRawSender + 'static,
        WireSm: Message + 'static,
        ApiSm: TryInto<WireSm> + Send + 'static,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
    {
        type Message = ApiSm;

        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            let buf = msg.try_into()?.encode_to_vec();
            let nbytes_sent = self.sender.send(&buf).await?;
            aperturec_metrics::builtins::tx_bytes(nbytes_sent);
            Ok(())
        }
    }

    pub struct Duplex<C: RawReceiver + RawSender, ApiRm, ApiSm, WireRm, WireSm> {
        common_rs: C,
        buf: [u8; 65536],
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<C: RawReceiver + RawSender, ApiRm, ApiSm, WireRm, WireSm>
        Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        pub fn new(common_rs: C) -> Self {
            Duplex {
                common_rs,
                buf: [0_u8; 65536],
                _api_rm: PhantomData,
                _api_sm: PhantomData,
                _wire_rm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> C {
            self.common_rs
        }
    }

    impl<C: RawReceiver + RawSender + Clone, ApiRm, ApiSm, WireRm, WireSm>
        Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        pub fn split(
            self,
        ) -> (
            ReceiverSimplex<C, ApiRm, WireRm>,
            SenderSimplex<C, ApiSm, WireSm>,
        ) {
            let wh = self.common_rs.clone();

            (
                ReceiverSimplex {
                    receiver: self.common_rs,
                    buf: self.buf,
                    _api_rm: PhantomData,
                    _wire_rm: PhantomData,
                },
                SenderSimplex {
                    sender: wh,
                    _api_sm: PhantomData,
                    _wire_sm: PhantomData,
                },
            )
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> Receiver for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: RawReceiver + RawSender,
        WireRm: Message + Default,
        ApiRm: TryFrom<WireRm>,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiRm;

        fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let nbytes_recvd = self.common_rs.receive(&mut self.buf)?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok((WireRm::decode(slice)?.try_into()?, nbytes_recvd))
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> Sender for Duplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: RawReceiver + RawSender,
        WireSm: Message,
        ApiSm: TryInto<WireSm>,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync + 'static,
    {
        type Message = ApiSm;
        fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            let buf = msg.try_into()?.encode_to_vec();
            let nbytes_sent = self.common_rs.send(&buf)?;
            aperturec_metrics::builtins::tx_bytes(nbytes_sent);
            Ok(())
        }
    }

    pub struct AsyncDuplex<C: AsyncRawReceiver + AsyncRawSender, ApiRm, ApiSm, WireRm, WireSm> {
        common_rs: C,
        buf: [u8; 65536],
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<C: AsyncRawReceiver + AsyncRawSender, ApiRm, ApiSm, WireRm, WireSm>
        AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    {
        pub fn new(common_rs: C) -> Self {
            AsyncDuplex {
                common_rs,
                buf: [0_u8; 65536],
                _api_rm: PhantomData,
                _api_sm: PhantomData,
                _wire_rm: PhantomData,
                _wire_sm: PhantomData,
            }
        }

        pub fn into_inner(self) -> C {
            self.common_rs
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncReceiver for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRawReceiver + AsyncRawSender + 'static,
        WireRm: Message + Default + 'static,
        ApiRm: TryFrom<WireRm> + Send + 'static,
        <ApiRm as TryFrom<WireRm>>::Error: Error + Send + Sync,
        WireSm: Send + 'static,
        ApiSm: Send + 'static,
    {
        type Message = ApiRm;

        async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)> {
            let nbytes_recvd = self.common_rs.receive(&mut self.buf).await?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok((WireRm::decode(slice)?.try_into()?, nbytes_recvd))
        }
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncSender for AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRawReceiver + AsyncRawSender + 'static,
        WireSm: Message + 'static,
        ApiSm: TryInto<WireSm> + Send + 'static,
        <ApiSm as TryInto<WireSm>>::Error: Error + Send + Sync,
        WireRm: Send + 'static,
        ApiRm: Send + 'static,
    {
        type Message = ApiSm;

        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()> {
            let buf = msg.try_into()?.encode_to_vec();
            let nbytes_sent = self.common_rs.send(&buf).await?;
            aperturec_metrics::builtins::tx_bytes(nbytes_sent);
            Ok(())
        }
    }

    pub type ServerMediaChannel = Duplex<
        udp::Server<udp::Connected>,
        media::client_to_server::Message,
        media::server_to_client::Message,
        media::ClientToServer,
        media::ServerToClient,
    >;

    pub type ClientMediaChannel = Duplex<
        udp::Client<udp::Connected>,
        media::server_to_client::Message,
        media::client_to_server::Message,
        media::ServerToClient,
        media::ClientToServer,
    >;

    pub type AsyncServerMediaChannel = AsyncDuplex<
        udp::Server<udp::AsyncConnected>,
        media::client_to_server::Message,
        media::server_to_client::Message,
        media::ClientToServer,
        media::ServerToClient,
    >;

    pub type AsyncClientMediaChannel = AsyncDuplex<
        udp::Client<udp::AsyncConnected>,
        media::server_to_client::Message,
        media::client_to_server::Message,
        media::ServerToClient,
        media::ClientToServer,
    >;
}

#[cfg(test)]
mod tcp_test {
    use super::reliable::*;
    use crate::reliable::*;
    use crate::{AsyncReceiver, AsyncSender, Receiver, Sender};

    use aperturec_protocol::common::*;
    use aperturec_protocol::control;
    use aperturec_protocol::event;
    use aperturec_state_machine::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn tcp_client_and_server<A: Into<SocketAddr>>(
        addr: A,
    ) -> (tcp::Client<tcp::Connected>, tcp::Server<tcp::Accepted>) {
        let addr = addr.into();
        let server = try_transition!(tcp::Server::new(addr.clone()), tcp::Listening)
            .expect("Failed to listen");
        let client = try_transition!(tcp::Client::new(addr), tcp::Connected)
            .expect("Failed to connect client");
        let server = try_transition!(server, tcp::Accepted).expect("Failed to accept server");
        (client, server)
    }

    async fn async_tcp_client_and_server<A: Into<SocketAddr>>(
        addr: A,
    ) -> (
        tcp::Client<tcp::AsyncConnected>,
        tcp::Server<tcp::AsyncAccepted>,
    ) {
        let addr = addr.into();
        let server = try_transition_async!(tcp::Server::new(addr.clone()), tcp::AsyncListening)
            .expect("Failed to listen");
        let client = try_transition_async!(tcp::Client::new(addr), tcp::AsyncConnected)
            .expect("Failed to connect client");
        let server =
            try_transition_async!(server, tcp::AsyncAccepted).expect("Failed to accept server");
        (client, server)
    }

    macro_rules! client_init {
        ($client_id:literal) => {{
            control::ClientInitBuilder::default()
                .temp_id($client_id)
                .client_info(
                    control::ClientInfoBuilder::default()
                        .version(SemVer::new(0, 1, 2))
                        .build_id("asdf")
                        .os(control::Os::Linux)
                        .os_version("Bionic Beaver")
                        .ssl_library("OpenSSL")
                        .ssl_version("1.2")
                        .bitness(control::Bitness::B64)
                        .endianness(control::Endianness::Big)
                        .architecture(control::Architecture::X86)
                        .cpu_id("Haswell")
                        .number_of_cores(4_u32)
                        .amount_of_ram("2.4Gb")
                        .display_size(Dimension::new(1024, 768))
                        .build()
                        .expect("ClientInfo build"),
                )
                .client_caps(ClientCaps {
                    supported_codecs: vec![Codec::Zlib.into()],
                })
                .client_heartbeat_interval::<prost_types::Duration>(
                    Duration::from_millis(1000_u64).try_into().unwrap(),
                )
                .client_heartbeat_response_interval::<prost_types::Duration>(
                    Duration::from_millis(1000_u64).try_into().unwrap(),
                )
                .max_decoder_count(1_u32)
                .client_specified_program_cmdline("glxgears")
                .build()
                .expect("ClientInit build")
        }};
    }

    macro_rules! server_init {
        () => {{
            control::ServerInitBuilder::default()
                .client_id(1_u32)
                .server_name("Some sweet server")
                .event_port(12345_u32)
                .display_size(Dimension::new(800, 600))
                .cursor_bitmaps(vec![
                    CursorBitmap {
                        cursor: Cursor::Default.into(),
                        data: vec![110, 20, 30],
                    },
                    CursorBitmap {
                        cursor: Cursor::Text.into(),
                        data: vec![11, 22, 33],
                    },
                ])
                .decoder_areas(vec![])
                .build()
                .expect("ServerInit build")
        }};
    }

    #[test]
    fn cc_client_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server(([127, 0, 0, 1], 9000));
        let mut server_cc = ServerControlChannel::new(tcp_server);
        let mut client_cc = ClientControlChannel::new(tcp_client);
        let ci: control::client_to_server::Message = client_init!(1234_u32).into();
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
        let (tcp_client, tcp_server) = async_tcp_client_and_server(([127, 0, 0, 1], 9001)).await;
        let mut server_cc = AsyncServerControlChannel::new(tcp_server);
        let mut client_cc = AsyncClientControlChannel::new(tcp_client);

        let ci: control::client_to_server::Message = client_init!(1234_u32).into();
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

    #[test]
    fn cc_server_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server(([127, 0, 0, 1], 9002));
        let mut server_cc = ServerControlChannel::new(tcp_server);
        let mut client_cc = ClientControlChannel::new(tcp_client);

        let si: control::server_to_client::Message = server_init!().into();
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
        let (tcp_client, tcp_server) = async_tcp_client_and_server(([127, 0, 0, 1], 9003)).await;
        let mut server_cc = AsyncServerControlChannel::new(tcp_server);
        let mut client_cc = AsyncClientControlChannel::new(tcp_client);

        let si: control::server_to_client::Message = server_init!().into();
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

    #[test]
    fn ec_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server(([127, 0, 0, 1], 9004));
        let mut server_ec = ServerEventChannel::new(tcp_server);
        let mut client_ec = ClientEventChannel::new(tcp_client);

        let msg: event::client_to_server::Message = event::KeyEvent { down: true, key: 1 }.into();
        client_ec.send(msg.clone()).expect("event channel send");
        let (rx, len) = server_ec.receive_with_len().expect("event channel receive");

        assert_eq!(msg, rx);
        assert_eq!(msg.encoded_len(), len);
    }

    #[tokio::test]
    async fn ec_init_async() {
        let (tcp_client, tcp_server) = async_tcp_client_and_server(([127, 0, 0, 1], 9005)).await;
        let mut server_ec = AsyncServerEventChannel::new(tcp_server);
        let mut client_ec = AsyncClientEventChannel::new(tcp_client);

        let msg: event::client_to_server::Message = event::KeyEvent { down: true, key: 1 }.into();
        client_ec
            .send(msg.clone())
            .await
            .expect("event channel send");
        let (rx, len) = server_ec
            .receive_with_len()
            .await
            .expect("event channel receive");

        assert_eq!(msg, rx);
        assert_eq!(msg.encoded_len(), len);
    }
}

#[cfg(test)]
mod udp_test {
    use super::unreliable::*;
    use crate::unreliable::*;
    use crate::{AsyncReceiver, AsyncSender, Receiver, Sender};

    use aperturec_protocol::common::*;
    use aperturec_protocol::media as mm;
    use aperturec_protocol::media::client_to_server as mm_c2s;
    use aperturec_protocol::media::server_to_client as mm_s2c;
    use aperturec_state_machine::*;
    use std::net::SocketAddr;
    use std::time::SystemTime;

    fn udp_client_and_server<A: Into<SocketAddr>>(
        addr: A,
    ) -> (udp::Client<udp::Connected>, udp::Server<udp::Listening>) {
        let addr = addr.into();
        let server = try_transition!(udp::Server::new(addr.clone()), udp::Listening)
            .expect("Failed to listen");
        let client = try_transition!(udp::Client::new(addr, None), udp::Connected)
            .expect("Failed to connect client");
        (client, server)
    }

    async fn async_udp_client_and_server<A: Into<SocketAddr>>(
        addr: A,
    ) -> (
        udp::Client<udp::AsyncConnected>,
        udp::Server<udp::AsyncListening>,
    ) {
        let addr = addr.into();
        let server = try_transition_async!(udp::Server::new(addr.clone()), udp::AsyncListening)
            .expect("Failed to listen");
        let client = try_transition_async!(udp::Client::new(addr, None), udp::AsyncConnected)
            .expect("Failed to connect client");
        (client, server)
    }

    macro_rules! test_c2s_media_message {
        () => {{
            mm::MediaKeepalive::from(Decoder::new(1337))
        }};
    }

    macro_rules! test_s2c_media_message {
        () => {{
            mm::FramebufferUpdate::new(
                vec![mm::RectangleUpdateBuilder::default()
                    .sequence_id(1_u64)
                    .location(Location::new(0, 0))
                    .rectangle(mm::Rectangle::new(
                        Codec::Raw.into(),
                        None,
                        vec![0xc5; 1024].into(),
                    ))
                    .build()
                    .expect("RectangleUpdate build")],
                1_u64,
                Some(SystemTime::now().into()),
            )
        }};
    }

    #[test]
    fn mc_init() {
        let (sync_client, sync_server) = udp_client_and_server(([127, 0, 0, 1], 10000));
        let mut client_mc = ClientMediaChannel::new(sync_client);
        let keepalive: mm_c2s::Message = test_c2s_media_message!().into();
        let msg: mm_s2c::Message = test_s2c_media_message!().into();
        client_mc
            .send(keepalive.clone())
            .expect("failed to send media keepalive 1");
        let mut server_mc = ServerMediaChannel::new(
            sync_server
                .try_transition()
                .expect("failed to connect server"),
        );
        server_mc
            .send(msg.clone())
            .expect("failed to send media message 1");
        let recvd = client_mc
            .receive()
            .expect("failed to receive media message 1");
        assert_eq!(msg, recvd);
        server_mc
            .send(msg.clone())
            .expect("failed to send media message 2");
        let recvd = client_mc
            .receive()
            .expect("failed to receive media message 2");
        assert_eq!(msg, recvd);
        client_mc
            .send(keepalive.clone())
            .expect("failed to send media keepalive 2");
        let (recvd, len) = server_mc
            .receive_with_len()
            .expect("failed to receive media keepalive 2");
        assert_eq!(keepalive, recvd);
        assert_eq!(keepalive.encoded_len(), len);

        let _sync_client = server_mc.into_inner();
        let _sync_server = client_mc.into_inner();
    }

    #[tokio::test]
    async fn mc_init_async() {
        let (async_client, async_server) =
            async_udp_client_and_server(([127, 0, 0, 1], 10001)).await;
        let mut client_mc = AsyncClientMediaChannel::new(async_client);
        let keepalive: mm_c2s::Message = test_c2s_media_message!().into();
        let msg: mm_s2c::Message = test_s2c_media_message!().into();
        client_mc
            .send(keepalive.clone())
            .await
            .expect("failed to send async media keepalive 1");
        let mut server_mc = AsyncServerMediaChannel::new(
            async_server
                .try_transition()
                .await
                .expect("failed to connect server"),
        );
        let recvd = server_mc
            .receive()
            .await
            .expect("failed to receive async media keepalive 1");
        assert_eq!(keepalive, recvd);
        server_mc
            .send(msg.clone())
            .await
            .expect("failed to send async media message 1");
        let recvd = client_mc
            .receive()
            .await
            .expect("failed to receive async media message 1");
        assert_eq!(msg, recvd);
        server_mc
            .send(msg.clone())
            .await
            .expect("failed to send async media message 2");
        let recvd = client_mc
            .receive()
            .await
            .expect("failed to receive async media message 2");
        assert_eq!(msg, recvd);
        client_mc
            .send(keepalive.clone())
            .await
            .expect("failed to send async media keepalive 2");
        let (recvd, len) = server_mc
            .receive_with_len()
            .await
            .expect("failed to receive async media keepalive 2");
        assert_eq!(keepalive, recvd);
        assert_eq!(keepalive.encoded_len(), len);

        let _async_client = server_mc.into_inner();
        let _async_server = client_mc.into_inner();
    }
}
