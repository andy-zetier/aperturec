/// A trait to gate how fast a [`Sender`] can send messages.
///
/// This trait assumes that the [`Sender`]'s messages will serialize to some number of bytes. The
/// [`Gate`] limits the number of bytes which can be sent at any given time.
pub trait Gate {
    /// Called before sending a message of [`msg_size`] bytes
    fn wait(&self, msg_size: usize) -> anyhow::Result<()>;
}

mod async_variants {
    #[trait_variant::make(Gate: Send + Sync)]
    #[allow(dead_code)]
    /// Async variant of [`super::Gate`]
    pub trait LocalGate {
        async fn wait(&self, msg_size: usize) -> anyhow::Result<()>;
    }
}
pub use async_variants::Gate as AsyncGate;
