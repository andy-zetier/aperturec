use aperturec_metrics::exporters::{
    CsvExporter, Exporter, LogExporter, PrometheusExporter, PushgatewayExporter,
};
use aperturec_metrics::{create_histogram_metric_with_buckets, create_metric, MetricsInitializer};
use aperturec_trace::{log, Level};

create_histogram_metric_with_buckets!(
    CompressionRatio,
    "%",
    prometheus::linear_buckets(11.0, 11.0, 9).unwrap()
);

create_metric!(PixelsCompressed);
create_metric!(TimeInCompression);
create_metric!(FramesCut);
create_metric!(EncoderCount);

create_histogram_metric_with_buckets!(
    TrackingBufferDamageRatio,
    "%",
    prometheus::linear_buckets(10., 10., 10).unwrap()
);

pub fn setup_server_metrics(
    metrics_log: bool,
    metrics_csv: Option<String>,
    metrics_pushgateway: Option<String>,
    metrics_prometheus: Option<String>,
    prom_id: u32,
) {
    if metrics_prometheus.is_none() && metrics_pushgateway.is_none() {
        log::warn!("Prometheus metrics are not being exported. Use --metrics-pushgateawy or --metrics-prometheus to resolve");
    }

    if metrics_log
        || metrics_csv.is_some()
        || metrics_pushgateway.is_some()
        || metrics_prometheus.is_some()
    {
        let mut exporters: Vec<Exporter> = vec![];
        if metrics_log {
            match LogExporter::new(Level::DEBUG) {
                Ok(le) => exporters.push(Exporter::Log(le)),
                Err(err) => log::warn!("Failed to setup Log exporter: {}, disabling", err),
            }
        }
        if let Some(path) = metrics_csv {
            match CsvExporter::new(path) {
                Ok(csve) => exporters.push(Exporter::Csv(csve)),
                Err(err) => log::warn!("Failed to setup CSV exporter: {}, disabling", err),
            }
        }
        if let Some(url) = metrics_pushgateway {
            match PushgatewayExporter::new(url.to_owned(), "aperturec_server".to_owned(), prom_id) {
                Ok(pge) => exporters.push(Exporter::Pushgateway(pge)),
                Err(err) => log::warn!("Failed to setup Pushgateway exporter: {}, disabling", err),
            }
        }
        if let Some(bind_addr) = metrics_prometheus {
            match PrometheusExporter::new(&bind_addr) {
                Ok(pe) => exporters.push(Exporter::Prometheus(pe)),
                Err(err) => log::warn!("Failed to setup Prometheus exporter: {}, disabling", err),
            }
        }

        MetricsInitializer::default()
            .with_poll_rate_from_secs(3)
            .with_exporters(exporters)
            .init()
            .expect("Failed to setup metrics");

        ctrlc::set_handler(move || {
            aperturec_metrics::stop();
            std::process::exit(0);
        })
        .unwrap();

        CompressionRatio::register();
        TrackingBufferDamageRatio::register();
        PixelsCompressed::register();
        TimeInCompression::register();
        FramesCut::register();
        EncoderCount::register();
    }
}
