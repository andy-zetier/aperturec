use std::time::Duration;

pub mod control;
pub mod event;
pub mod media;
pub mod tunnel;

use tracing::warn;

/// Timeout for inter-thread communication channel operations.
pub const ITC_TIMEOUT: Duration = Duration::from_millis(1);
/// Timeout for network channel operations.
pub const NETWORK_CHANNEL_TIMEOUT: Duration = Duration::from_millis(1);

/// Extension trait for channel senders that provides convenient error handling.
///
/// This trait extends crossbeam channel senders with a method that logs warnings
/// instead of propagating errors when sends fail.
pub trait SenderExt<T> {
    /// Sends a message on the channel, logging a warning if the send fails.
    ///
    /// Unlike the standard `send` method, this does not return an error.
    /// Instead, it logs a warning when the receiver has been dropped or disconnected.
    fn send_warn(&self, msg: T);
}

impl<T> SenderExt<T> for crossbeam::channel::Sender<T> {
    fn send_warn(&self, msg: T) {
        self.send(msg)
            .unwrap_or_else(|e| warn!(%e, "failed to send internal message"));
    }
}
