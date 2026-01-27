use aperturec_metrics::{create_histogram_metric_with_buckets, create_metric};
use std::sync::Once;

static CLIENT_METRICS_REGISTERED: Once = Once::new();

create_metric!(RefreshCount);

create_histogram_metric_with_buckets!(
    EventChannelSendLatency,
    "ms",
    prometheus::exponential_buckets(1.0, 2.0, 14).unwrap()
);

pub fn setup_client_metrics() {
    EventChannelSendLatency::register();
    RefreshCount::register();
}

pub(crate) fn ensure_client_metrics_registered() {
    CLIENT_METRICS_REGISTERED.call_once(setup_client_metrics);
}
