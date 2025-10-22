//! High-level, message-oriented API
use std::error::Error;

pub mod in_order;
pub mod out_of_order;

/// A trait for types which receive messages
pub trait Receiver {
    /// The type of message which is received
    type Message;

    /// The error type returned when receiving fails
    type Error: Error;

    /// Block until a message is available or an error occurs, and return the message and number of
    /// bytes the message consists of
    fn receive_with_len(&mut self) -> Result<(Self::Message, usize), Self::Error>;

    /// Block until a message is available or an error occurs, and return the message
    fn receive(&mut self) -> Result<Self::Message, Self::Error> {
        let (msg, _) = self.receive_with_len()?;
        Ok(msg)
    }
}

/// A trait for types which send messages
pub trait Sender {
    /// The type of message which is sent
    type Message;

    /// The error type returned when sending fails
    type Error: Error;

    /// Send a message, potentially blocking and returning an error one occurred
    fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error>;
}

/// A trait for all type which can flush messages, blocking until the messages which have been sent
/// have been received by the other end.
pub trait Flushable: Sender {
    /// Ensure all messages which have been sent are received by the other side
    fn flush(&mut self) -> Result<(), Self::Error>;
}

/// A unifying trait for types which are both [`Sender`]s and [`Receiver`]s.
///
/// This trait should never be implemented by hand as the blanket implementation covers all cases
pub trait Duplex: Sender + Receiver {}

impl<T: Sender + Receiver> Duplex for T {}

mod async_variants {
    use futures::sink::{self, Sink};
    use futures::stream::{self, Stream};
    use futures::{Future, FutureExt};
    use std::error::Error;

    #[trait_variant::make(Receiver: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Receiver`]
    pub trait LocalReceiver: Send + Sized {
        type Message: Send;
        type Error: Error;

        fn receive(&mut self) -> impl Future<Output = Result<Self::Message, Self::Error>> {
            self.receive_with_len().map(|res| res.map(|tup| tup.0))
        }

        async fn receive_with_len(&mut self) -> Result<(Self::Message, usize), Self::Error>;

        fn stream(self) -> impl Stream<Item = Result<Self::Message, Self::Error>> {
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
        type Error: Error;

        async fn send(&mut self, msg: Self::Message) -> Result<(), Self::Error>;

        fn sink(self) -> impl Sink<Self::Message, Error = Self::Error> {
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
        async fn flush(&mut self) -> Result<(), Self::Error>;
    }

    #[trait_variant::make(Duplex: Send)]
    #[allow(dead_code)]
    /// Async variant of [`super::Duplex`]
    pub trait LocalDuplex: Sender + Receiver {}
    impl<T: Sender + Receiver> Duplex for T {}
}
pub use async_variants::Duplex as AsyncDuplex;
pub use async_variants::Flushable as AsyncFlushable;
pub use async_variants::Receiver as AsyncReceiver;
pub use async_variants::Sender as AsyncSender;
