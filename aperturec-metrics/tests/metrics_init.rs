use aperturec_metrics::MetricsInitializer;
use aperturec_metrics::exporters::{Exporter, LogExporter};
use aperturec_metrics::metrics_initialized;
use tracing::Level;

#[test]
fn metrics_initializer_reports_initialized_and_rejects_reinit() {
    let already_initialized = metrics_initialized();

    let exporter = Exporter::Log(LogExporter::new(Level::INFO).expect("log exporter"));
    let init_result = MetricsInitializer::default().with_exporter(exporter).init();

    if already_initialized {
        assert!(init_result.is_err());
        return;
    }

    assert!(init_result.is_ok());
    assert!(metrics_initialized());

    let exporter = Exporter::Log(LogExporter::new(Level::INFO).expect("log exporter"));
    let reinit_result = MetricsInitializer::default().with_exporter(exporter).init();

    assert!(reinit_result.is_err());
}
