use aperturec_metrics::{create_histogram_metric_with_buckets, create_metric};

create_histogram_metric_with_buckets!(
    CompressionRatio,
    "%",
    prometheus::linear_buckets(11.0, 11.0, 9).unwrap()
);

create_metric!(PixelsCompressed);
create_metric!(FramesCut);
create_metric!(EncoderCount);
create_metric!(TrackingBufferDisjointAreas);
create_metric!(TrackingBufferUpdates);
create_metric!(BackendEvent);
create_metric!(ClientActivityEvent);
create_metric!(DisplayWidth);
create_metric!(DisplayHeight);

create_histogram_metric_with_buckets!(
    TimeInCompression,
    "ms",
    prometheus::exponential_buckets(1.0, 2.0, 14).unwrap()
);

create_histogram_metric_with_buckets!(
    FrameSyncPermitWaitLatency,
    "ms",
    prometheus::exponential_buckets(1.0, 2.0, 14).unwrap()
);

create_histogram_metric_with_buckets!(
    TrackingBufferDamageRatio,
    "%",
    prometheus::linear_buckets(10., 10., 10).unwrap()
);

pub fn setup_server_metrics() {
    CompressionRatio::register();
    TrackingBufferDamageRatio::register();
    PixelsCompressed::register();
    TimeInCompression::register();
    FramesCut::register();
    FrameSyncPermitWaitLatency::register();
    EncoderCount::register();
    TrackingBufferDisjointAreas::register();
    TrackingBufferUpdates::register();
    BackendEvent::register();
    ClientActivityEvent::register();
    DisplayWidth::register();
    DisplayHeight::register();
}
