pub mod reliable {
    use crate::reliable::*;
    use crate::*;

    use aperturec_protocol::*;
    use prost::Message;
    use std::error::Error;
    use std::io::{self, BufRead, Read, Write};
    use std::marker::PhantomData;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};

    const BUF_READER_CAPACITY: usize = 4096 * 32;

    fn fill_buf_reader<R: Read>(reader: &mut std::io::BufReader<R>) -> anyhow::Result<()> {
        let curr_bytes_in_buffer = reader.buffer().len();
        match reader.fill_buf() {
            Ok(buffer) => {
                aperturec_metrics::builtins::rx_bytes(buffer.len() - curr_bytes_in_buffer);
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::Interrupted && e.kind() != io::ErrorKind::WouldBlock {
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    fn do_receive<R: Read, RM: Message + Default>(
        reader: &mut std::io::BufReader<R>,
    ) -> anyhow::Result<RM> {
        if reader.buffer().is_empty() {
            fill_buf_reader(reader)?;
        }

        let msg_length = loop {
            match prost::decode_length_delimiter(reader.buffer()) {
                Ok(msg_length) => break msg_length,
                Err(decode_err) => {
                    if reader.buffer().len() >= 10 {
                        Err(decode_err)?;
                    }

                    fill_buf_reader(reader)?;
                }
            }
        };

        let length_delimiter_len = prost::length_delimiter_len(msg_length);
        reader.consume(length_delimiter_len);

        while reader.buffer().len() < msg_length {
            fill_buf_reader(reader)?;
        }

        let msg = RM::decode(&reader.buffer()[..msg_length])?;
        reader.consume(msg_length);
        Ok(msg)
    }

    fn do_send<W: Write, SM: Message>(
        writer: &mut W,
        msg: SM,
        write_bytes: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        write_bytes.extend(msg.encode_length_delimited_to_vec());

        while !write_bytes.is_empty() {
            match writer.write(write_bytes) {
                Ok(nbytes_written) => {
                    aperturec_metrics::builtins::tx_bytes(nbytes_written);
                    write_bytes.drain(..nbytes_written);
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock
                        && e.kind() != io::ErrorKind::Interrupted
                    {
                        return Err(e.into());
                    }
                }
            }
        }
        writer.flush()?;
        Ok(())
    }

    async fn fill_buf_reader_async<R: AsyncRead + Unpin>(
        reader: &mut tokio::io::BufReader<R>,
    ) -> anyhow::Result<()> {
        let curr_bytes_in_buffer = reader.buffer().len();
        reader.fill_buf().await?;
        aperturec_metrics::builtins::rx_bytes(reader.buffer().len() - curr_bytes_in_buffer);
        Ok(())
    }

    async fn do_receive_async<R: AsyncRead + Unpin, RM: Message + Default>(
        reader: &mut tokio::io::BufReader<R>,
    ) -> anyhow::Result<RM> {
        let msg_length = loop {
            match prost::decode_length_delimiter(reader.buffer()) {
                Ok(msg_length) => break msg_length,
                Err(e) => {
                    if reader.buffer().len() >= 10 {
                        return Err(e.into());
                    }

                    fill_buf_reader_async(reader).await?;
                }
            }
        };
        let length_delimiter_len = prost::length_delimiter_len(msg_length);
        reader.consume(length_delimiter_len);

        while reader.buffer().len() < msg_length {
            fill_buf_reader_async(reader).await?;
        }
        let msg = RM::decode(&reader.buffer()[..msg_length])?;
        reader.consume(msg_length);
        Ok(msg)
    }

    async fn do_send_async<W: AsyncWrite + Unpin, SM: Message>(
        writer: &mut W,
        msg: SM,
    ) -> anyhow::Result<()> {
        let mut buf = msg.encode_length_delimited_to_vec();
        let total_write_size = buf.len();
        let mut buf_slice: &[u8] = &mut buf;
        let mut nbytes_written = 0;
        loop {
            nbytes_written += writer.write_buf(&mut buf_slice).await?;
            aperturec_metrics::builtins::tx_bytes(nbytes_written);
            if nbytes_written >= total_write_size {
                break;
            }
            buf_slice = &buf_slice[nbytes_written..];
        }
        writer.flush().await?;
        Ok(())
    }

    pub struct ReceiverSimplex<R: Read, ApiRm, WireRm> {
        reader: std::io::BufReader<R>,
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: Read, ApiRm, WireRm> ReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(reader: R) -> Self {
            ReceiverSimplex {
                reader: std::io::BufReader::with_capacity(BUF_READER_CAPACITY, reader),
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

        fn receive(&mut self) -> anyhow::Result<Self::Message> {
            Ok(do_receive::<R, WireRm>(&mut self.reader)?.try_into()?)
        }
    }

    pub struct AsyncReceiverSimplex<R: AsyncRead, ApiRm, WireRm> {
        reader: tokio::io::BufReader<R>,
        _api_rm: PhantomData<ApiRm>,
        _wire_rm: PhantomData<WireRm>,
    }

    impl<R: AsyncRead, ApiRm, WireRm> AsyncReceiverSimplex<R, ApiRm, WireRm> {
        pub fn new(reader: R) -> Self {
            AsyncReceiverSimplex {
                reader: tokio::io::BufReader::with_capacity(BUF_READER_CAPACITY, reader),
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

        async fn receive(&mut self) -> anyhow::Result<Self::Message> {
            Ok(do_receive_async::<R, WireRm>(&mut self.reader)
                .await?
                .try_into()?)
        }
    }

    pub struct SenderSimplex<W: Write, ApiSm, WireSm> {
        writer: W,
        write_bytes: Vec<u8>,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<W: Write, ApiSm, WireSm> SenderSimplex<W, ApiSm, WireSm> {
        pub fn new(writer: W) -> Self {
            SenderSimplex {
                writer,
                write_bytes: vec![],
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
            do_send::<W, WireSm>(&mut self.writer, msg.try_into()?, &mut self.write_bytes)
        }
    }

    pub struct AsyncSenderSimplex<W: AsyncWrite, ApiSm, WireSm> {
        writer: W,
        _api_sm: PhantomData<ApiSm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<W: AsyncWrite, ApiSm, WireSm> AsyncSenderSimplex<W, ApiSm, WireSm> {
        pub fn new(writer: W) -> Self {
            AsyncSenderSimplex {
                writer,
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
            do_send_async::<W, WireSm>(&mut self.writer, msg.try_into()?).await
        }
    }

    pub struct Duplex<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> {
        common_rw: std::io::BufReader<C>,
        write_bytes: Vec<u8>,
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<C: Read + Write, ApiRm, ApiSm, WireRm, WireSm> Duplex<C, ApiRm, ApiSm, WireRm, WireSm> {
        pub fn new(common_rw: C) -> Self {
            Duplex {
                common_rw: std::io::BufReader::with_capacity(BUF_READER_CAPACITY, common_rw),
                write_bytes: vec![],
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
                    _api_rm: PhantomData,
                    _wire_rm: PhantomData,
                },
                SenderSimplex {
                    writer: wh,
                    write_bytes: self.write_bytes,
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
        fn receive(&mut self) -> anyhow::Result<ApiRm> {
            Ok(do_receive::<C, WireRm>(&mut self.common_rw)?.try_into()?)
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
                &mut self.write_bytes,
            )
        }
    }

    pub struct AsyncDuplex<C: AsyncRead + AsyncWrite, ApiRm, ApiSm, WireRm, WireSm> {
        common_rw: tokio::io::BufReader<C>,
        _api_rm: PhantomData<ApiRm>,
        _api_sm: PhantomData<ApiSm>,
        _wire_rm: PhantomData<WireRm>,
        _wire_sm: PhantomData<WireSm>,
    }

    impl<C, ApiRm, ApiSm, WireRm, WireSm> AsyncDuplex<C, ApiRm, ApiSm, WireRm, WireSm>
    where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        pub fn new(common_rw: C) -> Self {
            AsyncDuplex {
                common_rw: tokio::io::BufReader::with_capacity(BUF_READER_CAPACITY, common_rw),
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
            AsyncSenderSimplex<tokio::io::WriteHalf<C>, ApiSm, WireSm>,
            AsyncReceiverSimplex<tokio::io::ReadHalf<C>, ApiRm, WireRm>,
        ) {
            let (rh, wh) = tokio::io::split(self.common_rw.into_inner());
            (AsyncSenderSimplex::new(wh), AsyncReceiverSimplex::new(rh))
        }

        pub fn unsplit(
            tx: AsyncSenderSimplex<tokio::io::WriteHalf<C>, ApiSm, WireSm>,
            rx: AsyncReceiverSimplex<tokio::io::ReadHalf<C>, ApiRm, WireRm>,
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
        async fn receive(&mut self) -> anyhow::Result<ApiRm> {
            Ok(do_receive_async::<C, WireRm>(&mut self.common_rw)
                .await?
                .try_into()?)
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
            do_send_async(&mut self.common_rw, msg.try_into()?).await
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
        fn receive(&mut self) -> anyhow::Result<Self::Message> {
            let nbytes_recvd = self.receiver.receive(&mut self.buf)?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok(WireRm::decode(slice)?.try_into()?)
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
            let nbytes_recvd = self.receiver.receive(&mut self.buf).await?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok(WireRm::decode(slice)?.try_into()?)
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
        fn receive(&mut self) -> anyhow::Result<Self::Message> {
            let nbytes_recvd = self.common_rs.receive(&mut self.buf)?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok(WireRm::decode(slice)?.try_into()?)
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

        async fn receive(&mut self) -> anyhow::Result<Self::Message> {
            let nbytes_recvd = self.common_rs.receive(&mut self.buf).await?;
            aperturec_metrics::builtins::rx_bytes(nbytes_recvd);
            let slice = &self.buf[0..nbytes_recvd];
            Ok(WireRm::decode(slice)?.try_into()?)
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
    use aperturec_state_machine::TryTransitionable;
    use std::net::SocketAddr;
    use std::time::Duration;

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
                .root_program("glxgears")
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

    #[tokio::test]
    async fn cc_client_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server!([127, 0, 0, 1], 9000);
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
        let (tcp_client, tcp_server) = async_tcp_client_and_server!([127, 0, 0, 1], 9001);
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

    #[tokio::test]
    async fn cc_server_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server!([127, 0, 0, 1], 9002);
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
        let (tcp_client, tcp_server) = async_tcp_client_and_server!([127, 0, 0, 1], 9003);
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

    #[tokio::test]
    async fn ec_init() {
        let (tcp_client, tcp_server) = tcp_client_and_server!([127, 0, 0, 1], 9004);
        let mut server_ec = ServerEventChannel::new(tcp_server);
        let mut client_ec = ClientEventChannel::new(tcp_client);

        let msg: event::client_to_server::Message = event::KeyEvent { down: true, key: 1 }.into();
        client_ec.send(msg.clone()).expect("event channel send");
        let rx = server_ec.receive().expect("event channel receive");

        assert_eq!(msg, rx);
    }

    #[tokio::test]
    async fn ec_init_async() {
        let (tcp_client, tcp_server) = async_tcp_client_and_server!([127, 0, 0, 1], 9005);
        let mut server_ec = AsyncServerEventChannel::new(tcp_server);
        let mut client_ec = AsyncClientEventChannel::new(tcp_client);

        let msg: event::client_to_server::Message = event::KeyEvent { down: true, key: 1 }.into();
        client_ec
            .send(msg.clone())
            .await
            .expect("event channel send");
        let rx = server_ec.receive().await.expect("event channel receive");

        assert_eq!(msg, rx);
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
    use aperturec_state_machine::TryTransitionable;
    use std::net::SocketAddr;

    macro_rules! udp_client_and_server {
        ($ip:expr, $port:expr) => {{
            let udp_server = udp::Server::new(SocketAddr::from(($ip, $port)))
                .try_transition()
                .await
                .expect("Failed to listen");
            let udp_client = udp::Client::new(
                SocketAddr::from(($ip, $port)),
                Some(SocketAddr::from(($ip, 0))),
            )
            .try_transition()
            .await
            .expect("Failed to connect");
            (udp_client, udp_server)
        }};
    }

    macro_rules! async_udp_client_and_sync_server {
        ($ip:expr, $port:expr) => {{
            let (sync_client, sync_server) = udp_client_and_server!($ip, $port);
            let async_client = sync_client
                .try_transition()
                .await
                .expect("Failed to make client async");
            (async_client, sync_server)
        }};
    }

    macro_rules! test_c2s_media_message {
        () => {{
            mm::MediaKeepalive::from(Decoder::new(1337))
        }};
    }

    macro_rules! test_s2c_media_message {
        () => {{
            mm::FramebufferUpdate::new(vec![mm::RectangleUpdateBuilder::default()
                .sequence_id(1_u64)
                .location(Location::new(0, 0))
                .rectangle(mm::Rectangle::new(
                    Codec::Raw.into(),
                    None,
                    vec![0xc5; 1024].into(),
                ))
                .build()
                .expect("RectangleUpdate build")])
        }};
    }

    #[tokio::test]
    async fn mc_init() {
        let (sync_client, sync_server) = udp_client_and_server!([127, 0, 0, 1], 10000);
        let mut rs_mc: ReceiverSimplex<
            udp::Server<udp::Listening>,
            mm_c2s::Message,
            mm::ClientToServer,
        > = ReceiverSimplex::new(sync_server);
        let mut client_mc = ClientMediaChannel::new(sync_client);
        let keepalive: mm_c2s::Message = test_c2s_media_message!().into();
        let msg: mm_s2c::Message = test_s2c_media_message!().into();
        client_mc
            .send(keepalive.clone())
            .expect("failed to send media keepalive 1");
        let recvd = rs_mc
            .receive()
            .expect("failed to receive media keepalive 1");
        assert_eq!(keepalive, recvd);
        let sync_server = rs_mc
            .into_inner()
            .try_transition()
            .await
            .expect("mc server connected");
        let mut server_mc = ServerMediaChannel::new(sync_server);
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
        let recvd = server_mc
            .receive()
            .expect("failed to receive media keepalive 2");
        assert_eq!(keepalive, recvd);

        let _sync_client = server_mc.into_inner();
        let _sync_server = client_mc.into_inner();
    }

    #[tokio::test]
    async fn mc_init_async() {
        let (async_client, sync_server) = async_udp_client_and_sync_server!([127, 0, 0, 1], 10001);
        let mut rs_mc: ReceiverSimplex<
            udp::Server<udp::Listening>,
            mm_c2s::Message,
            mm::ClientToServer,
        > = ReceiverSimplex::new(sync_server);
        let mut client_mc = AsyncClientMediaChannel::new(async_client);
        let keepalive: mm_c2s::Message = test_c2s_media_message!().into();
        let msg: mm_s2c::Message = test_s2c_media_message!().into();
        client_mc
            .send(keepalive.clone())
            .await
            .expect("failed to send async media keepalive 1");
        let recvd = rs_mc
            .receive()
            .expect("failed to receive async media keepalive 1");
        assert_eq!(keepalive, recvd);
        let async_server = rs_mc
            .into_inner()
            .try_transition()
            .await
            .expect("Connected")
            .try_transition()
            .await
            .expect("Async Server");
        let mut server_mc = AsyncServerMediaChannel::new(async_server);
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
        let recvd = server_mc
            .receive()
            .await
            .expect("failed to receive async media keepalive 2");
        assert_eq!(keepalive, recvd);

        let _async_client = server_mc.into_inner();
        let _async_server = client_mc.into_inner();
    }
}
