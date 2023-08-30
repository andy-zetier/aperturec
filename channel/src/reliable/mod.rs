use async_trait::async_trait;
use futures::sink::{self, Sink, SinkExt};
use futures::stream::{self, Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

pub mod tcp;

pub trait Receiver {
    type Message;
    fn receive(&mut self) -> anyhow::Result<Self::Message>;
}

#[async_trait]
pub trait AsyncReceiver: Send + Sized + 'static {
    type Message;

    async fn receive(&mut self) -> anyhow::Result<Self::Message>;

    fn stream(self) -> ReceiverStream<Self> {
        let stream = stream::unfold(self, |mut receiver| async move {
            let msg = receiver.receive().await.ok()?;
            Some((msg, receiver))
        });
        ReceiverStream {
            inner: Box::pin(stream),
        }
    }
}

pub struct ReceiverStream<R: AsyncReceiver> {
    inner: Pin<Box<dyn Stream<Item = R::Message>>>,
}

impl<R: AsyncReceiver> Stream for ReceiverStream<R> {
    type Item = R::Message;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
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

    fn sink(self) -> SenderSink<Self> {
        let sink = sink::unfold(self, |mut sender, msg| async {
            sender.send(msg).await?;
            Ok(sender)
        });
        SenderSink {
            inner: Box::pin(sink),
        }
    }
}

pub struct SenderSink<T: AsyncSender> {
    inner: Pin<Box<dyn Sink<T::Message, Error = anyhow::Error>>>,
}

impl<T: AsyncSender> Sink<T::Message> for SenderSink<T> {
    type Error = anyhow::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T::Message) -> Result<(), Self::Error> {
        self.inner.start_send_unpin(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_close_unpin(cx)
    }
}
