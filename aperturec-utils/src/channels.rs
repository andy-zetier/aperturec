use tracing::*;

/// Extension trait for channel senders that provides convenient error handling.
///
/// This trait extends crossbeam channel senders with a method that logs warnings
/// instead of propagating errors when sends fail.
pub trait SenderExt<T> {
    /// Sends a message on the channel, logging a warning if the send fails.
    ///
    /// Unlike the standard `send` method, this does not return an error.
    /// Instead, it logs a warning when the receiver has been dropped or disconnected.
    fn send_or_warn(&self, msg: T);
}

impl<T> SenderExt<T> for crossbeam::channel::Sender<T> {
    fn send_or_warn(&self, msg: T) {
        self.send(msg).unwrap_or_else(|e| {
            warn!(%e, "failed to send internal message");
        });
    }
}

impl<T> SenderExt<T> for async_channel::Sender<T> {
    fn send_or_warn(&self, msg: T) {
        self.send_blocking(msg).unwrap_or_else(|e| {
            warn!(%e, "failed to send internal message");
        });
    }
}

/// Extension trait for async channel senders that provides convenient error handling.
///
/// This trait mirrors [`SenderExt`] but exposes an `async fn` to integrate
/// naturally with async tasks.
#[allow(async_fn_in_trait)]
pub trait AsyncSenderExt<T: Send> {
    /// Sends a message on the channel, logging a warning if the send fails.
    ///
    /// Unlike the standard `send` method, this never returns an error. Instead,
    /// it logs a warning when the receiver has been dropped or disconnected.
    async fn send_or_warn(&self, msg: T);
}

impl<T: Send> AsyncSenderExt<T> for async_channel::Sender<T> {
    async fn send_or_warn(&self, msg: T) {
        if let Err(e) = self.send(msg).await {
            warn!(%e, "failed to send internal message");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_value: T) {}

    #[test]
    fn async_sender_future_is_send() {
        let (tx, _rx) = async_channel::bounded::<u8>(1);
        assert_send(AsyncSenderExt::send_or_warn(&tx, 42u8));
    }
}
