//! High-level, message-oriented API
use aperturec_protocol::{control, event, media, tunnel};

pub mod in_order;
pub mod out_of_order;

/// A trait for types which receive messages
pub trait Receiver {
    /// The type of message which is received
    type Message;

    /// Block until a message is available or an error occurs, and return the message and number of
    /// bytes the message consists of
    fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)>;

    /// Block until a message is available or an error occurs, and return the message
    fn receive(&mut self) -> anyhow::Result<Self::Message> {
        let (msg, _) = self.receive_with_len()?;
        Ok(msg)
    }
}

/// A trait for types which send messages
pub trait Sender {
    /// The type of message which is sent
    type Message;

    /// Send a message, potentially blocking and returning an error one occurred
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;
}

/// A trait for all type which can flush messages, blocking until the messages which have been sent
/// have been received by the other end.
pub trait Flushable: Sender {
    /// Ensure all messages which have been sent are received by the other side
    fn flush(&mut self) -> anyhow::Result<()>;
}

/// A unifying trait for types which are both [`Sender`]s and [`Receiver`]s.
///
/// This trait should never be implemented by hand as the blanket implementation covers all cases
pub trait Duplex: Sender + Receiver {}

impl<T: Sender + Receiver> Duplex for T {}
/// A trait for the Client-side of a channel, consisting of all channel types
pub trait UnifiedClient {
    type Control: Duplex
        + Sender<Message = control::client_to_server::Message>
        + Receiver<Message = control::server_to_client::Message>;
    type Event: Duplex
        + Sender<Message = event::client_to_server::Message>
        + Receiver<Message = event::server_to_client::Message>;
    type Media: Receiver<Message = media::ServerToClient>;
    type Tunnel: Duplex + Sender<Message = tunnel::Message> + Receiver<Message = tunnel::Message>;

    /// A leftover type that may be required to unsplit the channels back into [`Self`]
    type Residual;

    /// Split the Client into all channel types
    fn split(
        self,
    ) -> (
        Self::Control,
        Self::Event,
        Self::Media,
        Self::Tunnel,
        Self::Residual,
    );

    /// Combine all channel types back into the original Client type
    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        tc: Self::Tunnel,
        residual: Self::Residual,
    ) -> Self;
}

/// A trait for the Server-side of a channel, consisting of all channel types
pub trait UnifiedServer {
    type Control: Duplex
        + Sender<Message = control::server_to_client::Message>
        + Receiver<Message = control::client_to_server::Message>;
    type Event: Duplex
        + Sender<Message = event::server_to_client::Message>
        + Receiver<Message = event::client_to_server::Message>;
    type Media: Sender<Message = media::ServerToClient>;
    type Tunnel: Duplex + Sender<Message = tunnel::Message> + Receiver<Message = tunnel::Message>;

    /// A leftover type that may be required to unsplit the channels back into [`Self`]
    type Residual;

    /// Split the Server into all channel types
    fn split(
        self,
    ) -> (
        Self::Control,
        Self::Event,
        Self::Media,
        Self::Tunnel,
        Self::Residual,
    );

    /// Combine all channel types back into the original Server type
    fn unsplit(
        cc: Self::Control,
        ec: Self::Event,
        mc: Self::Media,
        tc: Self::Tunnel,
        residual: Self::Residual,
    ) -> Self;
}

mod async_variants {
    use super::*;
    use futures::sink::{self, Sink};
    use futures::stream::{self, Stream};
    use futures::{Future, FutureExt};

    #[trait_variant::make(Receiver: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Receiver`]
    pub trait LocalReceiver: Send + Sized {
        type Message: Send;

        fn receive(&mut self) -> impl Future<Output = anyhow::Result<Self::Message>> {
            self.receive_with_len().map(|res| res.map(|tup| tup.0))
        }

        async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)>;

        fn stream(self) -> impl Stream<Item = anyhow::Result<Self::Message>> {
            stream::unfold(self, |mut receiver| async move {
                Some((receiver.receive().await, receiver))
            })
        }
    }

    #[trait_variant::make(Sender: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Sender`]
    pub trait LocalSender: Send + Sized {
        type Message: Send;

        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;

        fn sink(self) -> impl Sink<Self::Message, Error = anyhow::Error> {
            sink::unfold(self, |mut sender, msg| async {
                sender.send(msg).await?;
                Ok(sender)
            })
        }
    }

    #[trait_variant::make(Flushable: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Flushable`]
    pub trait LocalFlushable: Sender + Send + Sized {
        async fn flush(&mut self) -> anyhow::Result<()>;
    }

    #[trait_variant::make(Duplex: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Duplex`]
    pub trait LocalDuplex: Sender + Receiver {}
    impl<T: Sender + Receiver> Duplex for T {}

    #[trait_variant::make(UnifiedClient: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::UnifiedClient`]
    pub trait LocalUnifiedClient {
        type Control: Duplex
            + Sender<Message = control::client_to_server::Message>
            + Receiver<Message = control::server_to_client::Message>;
        type Event: Duplex
            + Sender<Message = event::client_to_server::Message>
            + Receiver<Message = event::server_to_client::Message>;
        type Media: Receiver<Message = media::ServerToClient>;
        type Tunnel: Duplex
            + Sender<Message = tunnel::Message>
            + Receiver<Message = tunnel::Message>;
        type Residual;

        fn split(
            self,
        ) -> (
            Self::Control,
            Self::Event,
            Self::Media,
            Self::Tunnel,
            Self::Residual,
        );
        fn unsplit(
            cc: Self::Control,
            ec: Self::Event,
            mc: Self::Media,
            tc: Self::Tunnel,
            residual: Self::Residual,
        ) -> Self;
    }

    #[trait_variant::make(UnifiedServer: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::UnifiedServer`]
    pub trait LocalUnifiedServer {
        type Control: Duplex
            + Sender<Message = control::server_to_client::Message>
            + Receiver<Message = control::client_to_server::Message>;
        type Event: Duplex
            + Sender<Message = event::server_to_client::Message>
            + Receiver<Message = event::client_to_server::Message>;
        type Media: Sender<Message = media::ServerToClient>;
        type Tunnel: Duplex
            + Sender<Message = tunnel::Message>
            + Receiver<Message = tunnel::Message>;
        type Residual;

        fn split(
            self,
        ) -> (
            Self::Control,
            Self::Event,
            Self::Media,
            Self::Tunnel,
            Self::Residual,
        );
        fn unsplit(
            cc: Self::Control,
            ec: Self::Event,
            mc: Self::Media,
            tc: Self::Tunnel,
            residual: Self::Residual,
        ) -> Self;
    }
}

pub use async_variants::Duplex as AsyncDuplex;
pub use async_variants::Flushable as AsyncFlushable;
pub use async_variants::Receiver as AsyncReceiver;
pub use async_variants::Sender as AsyncSender;
pub use async_variants::UnifiedClient as AsyncUnifiedClient;
pub use async_variants::UnifiedServer as AsyncUnifiedServer;
