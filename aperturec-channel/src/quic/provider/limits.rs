use s2n_quic::provider::limits::Limits;
use std::time::Duration;

const DEFAULT_MAX_HANDSHAKE_DURATION: Duration = Duration::from_millis(250);

pub fn default() -> Limits {
    Limits::new()
        .with_max_handshake_duration(DEFAULT_MAX_HANDSHAKE_DURATION)
        .expect("handshake duration")
}
