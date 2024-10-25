use aperturec_metrics::exporters::*;

use tracing::*;

#[derive(Debug, clap::Args)]
pub struct MetricsArgGroup {
    /// Log metric data to a CSV file at the provided path
    #[arg(long, default_value = None)]
    metrics_csv: Option<String>,

    /// Log metric data at the DEBUG level (-vvv)
    #[arg(long)]
    metrics_log: bool,

    /// Serve Prometheus metrics at the given (optional) bind address. Defaults to 127.0.0.1:8080
    #[arg(long, num_args = 0..=1, default_value = None, default_missing_value = "127.0.0.1:8080")]
    metrics_prometheus: Option<String>,

    /// Send metric data to Pushgateway instance at the provided URL
    #[arg(long, default_value = None)]
    metrics_pushgateway: Option<String>,
}

impl MetricsArgGroup {
    pub fn to_exporters(self, name: &str) -> Vec<Exporter> {
        let mut exporters: Vec<Exporter> = vec![];
        if self.metrics_log {
            match LogExporter::new(Level::DEBUG) {
                Ok(le) => exporters.push(Exporter::Log(le)),
                Err(err) => warn!("Failed to setup Log exporter: {}, disabling", err),
            }
        }
        if let Some(path) = self.metrics_csv {
            match CsvExporter::new(path) {
                Ok(csve) => exporters.push(Exporter::Csv(csve)),
                Err(err) => warn!("Failed to setup CSV exporter: {}, disabling", err),
            }
        }
        if let Some(url) = self.metrics_pushgateway {
            match PushgatewayExporter::new(url.to_owned(), name.to_owned(), std::process::id()) {
                Ok(pge) => exporters.push(Exporter::Pushgateway(pge)),
                Err(err) => warn!("Failed to setup Pushgateway exporter: {}, disabling", err),
            }
        }
        if let Some(bind_addr) = self.metrics_prometheus {
            match PrometheusExporter::new(&bind_addr) {
                Ok(pe) => exporters.push(Exporter::Prometheus(pe)),
                Err(err) => warn!("Failed to setup Prometheus exporter: {}, disabling", err),
            }
        }
        exporters
    }
}
