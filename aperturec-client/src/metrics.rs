use aperturec_metrics::create_metric;

create_metric!(RefreshCount);

pub fn setup_client_metrics() {
    RefreshCount::register();
}
