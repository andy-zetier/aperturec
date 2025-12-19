use aperturec_client::{MetricsError, init_metrics, metrics_initialized};
use aperturec_metrics::exporters::{Exporter as MetricsExporter, LogExporter};
use tracing::Level;

/// Ensure metrics init enforces exporters, flips the global flag once, and rejects re-init.
#[test]
fn metrics_initialization_flow() {
    // 0) An empty exporter list must error and leave the flag unchanged.
    let was_initialized = metrics_initialized();
    assert!(
        !was_initialized,
        "initialization succeeded without any exporters"
    );
    let empty_result = init_metrics(vec![]);
    assert!(matches!(empty_result, Err(MetricsError::NoExporters)));
    assert_eq!(
        metrics_initialized(),
        was_initialized,
        "empty init must not change global state"
    );

    // 1) First real init succeeds and sets the process-global flag.
    let first = init_metrics(vec![MetricsExporter::Log(
        LogExporter::new(Level::DEBUG).expect("LogExporter must construct"),
    )]);
    assert!(first.is_ok(), "first metrics init should succeed");
    assert!(metrics_initialized(), "successful init must flip the flag");

    // 2) Any subsequent init (or init when already initialized) must error.
    let second = init_metrics(vec![MetricsExporter::Log(
        LogExporter::new(Level::DEBUG).expect("LogExporter must construct"),
    )]);
    assert!(
        matches!(second, Err(MetricsError::AlreadyInitialized)),
        "re-initialization should be rejected"
    );
}
