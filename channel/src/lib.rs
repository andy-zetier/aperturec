pub mod codec;
pub mod reliable;
pub mod tls;
pub mod unreliable;

pub type ServerControlChannel = codec::reliable::ServerControlChannel;
pub type ClientControlChannel = codec::reliable::ClientControlChannel;
pub type ServerEventChannel = codec::reliable::ServerEventChannel;
pub type ClientEventChannel = codec::reliable::ClientEventChannel;
pub type ServerMediaChannel = codec::unreliable::ServerMediaChannel;
pub type ClientMediaChannel = codec::unreliable::ClientMediaChannel;
pub type AsyncServerControlChannel = codec::reliable::AsyncServerControlChannel;
pub type AsyncServerControlChannelReadHalf = codec::reliable::AsyncServerControlChannelReadHalf;
pub type AsyncServerControlChannelWriteHalf = codec::reliable::AsyncServerControlChannelWriteHalf;
pub type AsyncClientControlChannel = codec::reliable::AsyncClientControlChannel;
pub type AsyncClientControlChannelReadHalf = codec::reliable::AsyncClientControlChannelReadHalf;
pub type AsyncClientControlChannelWriteHalf = codec::reliable::AsyncClientControlChannelWriteHalf;
pub type AsyncServerEventChannel = codec::reliable::AsyncServerEventChannel;
pub type AsyncClientEventChannel = codec::reliable::AsyncClientEventChannel;
pub type AsyncServerMediaChannel = codec::unreliable::AsyncServerMediaChannel;
pub type AsyncClientMediaChannel = codec::unreliable::AsyncClientMediaChannel;

pub trait Receiver {
    type Message;
    fn receive(&mut self) -> anyhow::Result<Self::Message> {
        let (msg, _) = self.receive_with_len()?;
        Ok(msg)
    }

    fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)>;
}

pub trait Sender {
    type Message;
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;
}

mod async_variants {
    use futures::sink::{self, Sink};
    use futures::stream::{self, Stream};
    use futures::{Future, FutureExt};

    #[trait_variant::make(Receiver: Send)]
    pub trait LocalReceiver: Send + Sized + 'static {
        type Message: Send;

        fn receive(&mut self) -> impl Future<Output = anyhow::Result<Self::Message>> {
            self.receive_with_len().map(|res| res.map(|tup| tup.0))
        }

        #[allow(async_fn_in_trait)]
        async fn receive_with_len(&mut self) -> anyhow::Result<(Self::Message, usize)>;

        fn stream(self) -> impl Stream<Item = anyhow::Result<Self::Message>> {
            stream::unfold(self, |mut receiver| async move {
                Some((receiver.receive().await, receiver))
            })
        }
    }

    #[trait_variant::make(Sender: Send)]
    pub trait LocalSender: Send + Sized + 'static {
        type Message: Send;

        #[allow(async_fn_in_trait)]
        async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;

        fn sink(self) -> impl Sink<Self::Message, Error = anyhow::Error> {
            sink::unfold(self, |mut sender, msg| async {
                sender.send(msg).await?;
                Ok(sender)
            })
        }
    }
}

pub use async_variants::Receiver as AsyncReceiver;
pub use async_variants::Sender as AsyncSender;
