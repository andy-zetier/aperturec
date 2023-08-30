use async_trait::async_trait;
use futures::sink::{self, Sink};
use futures::stream::{self, Stream};

pub mod codec;
pub mod reliable;
pub mod unreliable;

pub type ServerControlChannel = codec::der::reliable::ServerControlChannel;
pub type ClientControlChannel = codec::der::reliable::ClientControlChannel;
pub type ServerEventChannel = codec::der::reliable::ServerEventChannel;
pub type ClientEventChannel = codec::der::reliable::ClientEventChannel;
pub type ServerMediaChannel = codec::der::unreliable::ServerMediaChannel;
pub type ClientMediaChannel = codec::der::unreliable::ClientMediaChannel;
pub type AsyncServerControlChannel = codec::der::reliable::AsyncServerControlChannel;
pub type AsyncClientControlChannel = codec::der::reliable::AsyncClientControlChannel;
pub type AsyncServerEventChannel = codec::der::reliable::AsyncServerEventChannel;
pub type AsyncClientEventChannel = codec::der::reliable::AsyncClientEventChannel;
pub type AsyncServerMediaChannel = codec::der::unreliable::AsyncServerMediaChannel;
pub type AsyncClientMediaChannel = codec::der::unreliable::AsyncClientMediaChannel;

pub trait Receiver {
    type Message;
    fn receive(&mut self) -> anyhow::Result<Self::Message>;
}

#[async_trait]
pub trait AsyncReceiver: Send + Sized + 'static {
    type Message;

    async fn receive(&mut self) -> anyhow::Result<Self::Message>;

    fn stream(self) -> Box<dyn Stream<Item = Self::Message>> {
        Box::new(stream::unfold(self, |mut receiver| async move {
            let msg = receiver.receive().await.ok()?;
            Some((msg, receiver))
        }))
    }
}

pub trait Sender {
    type Message;
    fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;
}

#[async_trait]
pub trait AsyncSender: Send + Sized + 'static {
    type Message;
    async fn send(&mut self, msg: Self::Message) -> anyhow::Result<()>;

    fn sink(self) -> Box<dyn Sink<Self::Message, Error = anyhow::Error>> {
        Box::new(sink::unfold(self, |mut sender, msg| async {
            sender.send(msg).await?;
            Ok(sender)
        }))
    }
}
