mod gtk_app;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::prelude::*;

pub fn main() -> Result<()> {
    let args = gtk_app::ClientArgs::parse();
    let (log_layer, _guard) = args.client.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("ApertureC Client Startup");

    let metrics_exporters = args.metrics.to_exporters(env!("CARGO_CRATE_NAME"));
    if !metrics_exporters.is_empty() {
        aperturec_metrics::MetricsInitializer::default()
            .with_poll_rate_from_secs(3)
            .with_exporters(metrics_exporters)
            .init()
            .expect("Failed to initialize metrics");
    }

    gtk_app::run(args)
}
