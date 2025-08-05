use aperturec_metrics::{create_histogram_metric_with_buckets, create_metric};

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
