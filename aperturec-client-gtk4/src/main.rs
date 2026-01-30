//! GTK4 client binary entry point for ApertureC.

/// GTK application module and client entry helpers.
mod gtk_app;

use anyhow::Result;
use std::process;
use tracing::*;
use tracing_subscriber::prelude::*;

fn describe_startup_error(error: &anyhow::Error) -> String {
    use aperturec_client::ConnectionError;

    if let Some(conn_error) = error.downcast_ref::<ConnectionError>() {
        return match conn_error {
            ConnectionError::ChannelConnect(_) => {
                "Failed to connect to the server. Verify the address and network connectivity, and ensure the server is running.".to_string()
            }
            ConnectionError::ChannelBuild(_) => {
                "Failed to initialize the network connection. Please check your system configuration.".to_string()
            }
            ConnectionError::Tls(_) => {
                "TLS initialization failed. Check your system TLS configuration.".to_string()
            }
            _ => conn_error.to_string(),
        };
    }

    error.to_string()
}

/// Parse CLI arguments, configure logging/metrics, and launch the GTK client.
pub fn main() -> Result<()> {
    let args = gtk_app::ClientArgs::auto()?;
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

    if let Err(error) = gtk_app::run(args) {
        let message = describe_startup_error(&error);
        error!(%message, "client startup failed");
        debug!(?error, "startup error details");
        gtk_app::show_startup_error(&message);
        process::exit(1);
    }

    Ok(())
}
