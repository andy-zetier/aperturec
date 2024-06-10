/// A trait to gate how fast a [`Sender`] can send messages.
///
/// This trait assumes that the [`Sender`]'s messages will serialize to some number of bytes. The
/// [`Gate`] limits the number of bytes which can be sent at any given time.
pub trait Gate {
    /// The message to be sent without any gating information stamped
    type UnstampedMessage;

    /// The message to be sent once gating information is stamped
    type StampedMessage;

    fn wait_and_stamp(&self, msg: Self::UnstampedMessage) -> anyhow::Result<Self::StampedMessage>;
}

mod async_variants {
    #[trait_variant::make(Gate: Send + Sync)]
    #[allow(dead_code)]
    /// Async variant of [`super::Gate`]
    pub trait LocalGate {
        type UnstampedMessage;

        type StampedMessage;

        async fn wait_and_stamp(
            &self,
            msg: Self::UnstampedMessage,
        ) -> anyhow::Result<Self::StampedMessage>;
    }
}
pub use async_variants::Gate as AsyncGate;
