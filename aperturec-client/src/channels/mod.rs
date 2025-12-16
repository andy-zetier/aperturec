use std::time::Duration;

pub mod control;
pub mod event;
pub mod media;
pub mod tunnel;

/// Short polling delay used by cross-thread coordination loops.
pub const ITC_TIMEOUT: Duration = Duration::from_millis(1);
/// Receive timeout applied to network-backed channel reads so loops can check stop flags.
pub const NETWORK_CHANNEL_TIMEOUT: Duration = Duration::from_millis(1);
