//! High-level, message-oriented API
use std::{error::Error, time::Duration};

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

/// A [`Receiver`] that can bound how long it waits for the next message.
///
/// # Timeout Semantics
///
/// Timeout methods return `Ok(Some(msg))` when a complete message is received within the timeout,
/// `Ok(None)` when the timeout expires before a complete message arrives, and `Err(e)` for
/// transport or decode errors. The timeout applies to receiving a complete message, which may
/// require multiple transport receives. Timeouts represent best-effort maximum wait times
pub trait TimeoutReceiver: Receiver {
    /// Wait for up to `timeout` for a message and the number of bytes it consumed, returning
    /// `Ok(None)` when the deadline expires first.
    fn receive_with_len_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(Self::Message, usize)>, Self::Error>;

    /// Wait for up to `timeout` for a message, returning `Ok(None)` when the deadline expires.
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<Self::Message>, Self::Error> {
        if let Some((msg, _)) = self.receive_with_len_timeout(timeout)? {
            Ok(Some(msg))
        } else {
            Ok(None)
        }
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

#[cfg(test)]
pub(crate) mod test_helpers {
    use crate::gate::{AsyncGate, Gate};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    // Common test message types
    #[derive(Clone, PartialEq, prost::Message)]
    pub(crate) struct SimpleMessage {
        #[prost(string, tag = "1")]
        pub(crate) data: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub(crate) struct LargeMessage {
        #[prost(bytes, tag = "1")]
        pub(crate) payload: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub(crate) struct EmptyMessage {}

    #[derive(Clone, PartialEq, prost::Message)]
    pub(crate) struct MessageWithOptional {
        #[prost(string, tag = "1")]
        pub(crate) required_field: String,

        #[prost(string, optional, tag = "2")]
        pub(crate) optional_field: Option<String>,
    }

    // Mock gate implementation for testing rate limiting
    #[derive(Clone)]
    pub(crate) struct MockGate {
        should_block: Arc<AtomicBool>,
        call_count: Arc<AtomicUsize>,
        total_bytes: Arc<AtomicUsize>,
    }

    impl MockGate {
        pub(crate) fn new() -> Self {
            Self {
                should_block: Arc::new(AtomicBool::new(false)),
                call_count: Arc::new(AtomicUsize::new(0)),
                total_bytes: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub(crate) fn set_blocking(&self, blocking: bool) {
            self.should_block.store(blocking, Ordering::SeqCst);
        }

        pub(crate) fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        pub(crate) fn get_total_bytes(&self) -> usize {
            self.total_bytes.load(Ordering::SeqCst)
        }
    }

    impl Gate for MockGate {
        fn wait(
            &self,
            msg_len: usize,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.total_bytes.fetch_add(msg_len, Ordering::SeqCst);

            if self.should_block.load(Ordering::SeqCst) {
                Err("gate blocked".into())
            } else {
                Ok(())
            }
        }
    }

    impl AsyncGate for MockGate {
        async fn wait(
            &self,
            msg_len: usize,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
            Gate::wait(self, msg_len)
        }
    }
}
