use std::time::Duration;

pub mod control;
pub mod event;
pub mod media;
pub mod tunnel;

/// Timeout for inter-thread communication channel operations.
pub const ITC_TIMEOUT: Duration = Duration::from_millis(1);
/// Timeout for network channel operations.
pub const NETWORK_CHANNEL_TIMEOUT: Duration = Duration::from_millis(1);
