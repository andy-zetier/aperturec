use crate::paths;

use anyhow::Result;
use file_rotate::{
    compression::Compression,
    suffix::{AppendTimestamp, FileLimit},
    ContentLimit, FileRotate,
};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::*;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{filter::Directive, fmt::format::FmtSpan, *};

#[derive(Debug, clap::Args)]
pub struct LogArgGroup {
    /// Log level verbosity for all log outputs. Multiple -v options increase the verbosity. The
    /// maximum is 3. This can be overridden using the RUST_LOG environment variable. Overrides all
    /// --log-*-directive arguments.
    #[arg(short, action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Disable logging to stderr
    #[arg(short, long, action = clap::ArgAction::SetTrue, conflicts_with_all = &["no_color", "log_stderr_directive"], default_value = "false")]
    quiet: bool,

    /// Disable colors when logging to stderr
    #[arg(long, action = clap::ArgAction::SetTrue, conflicts_with = "quiet", default_value = "false")]
    no_color: bool,

    /// stderr logging directive. Follows the format of RUST_LOG/EnvFilter
    #[arg(long, conflicts_with = "quiet", default_value = "warn")]
    log_stderr_directive: String,

    /// Disable logging to journald
    #[arg(long, action = clap::ArgAction::SetTrue, default_value = "false")]
    #[cfg(unix)]
    no_log_journald: bool,

    /// journald logging directive. Follows the format of RUST_LOG/EnvFilter
    #[arg(long, conflicts_with = "no_log_journald", default_value = "info")]
    #[cfg(unix)]
    log_journald_directive: String,

    /// Disable logging to files
    #[arg(long, action = clap::ArgAction::SetTrue, default_value = "false")]
    no_log_file: bool,

    /// The directory to write log files to
    #[arg(long, default_value_os_t = paths::log_dir(), conflicts_with = "no_log_file")]
    log_file_directory: PathBuf,

    /// file logging directive. Follows the format of RUST_LOG/EnvFilter
    #[arg(long, conflicts_with = "no_log_file", default_value = "info")]
    log_file_directive: String,
}

pub struct Guard {
    _inner: Vec<WorkerGuard>,
}

impl LogArgGroup {
    pub fn as_tracing_layer<S>(&self) -> Result<(impl Layer<S> + Send + Sync, Guard)>
    where
        S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        let make_filter = |arg_verbosity, arg_directive| {
            let verbosity = match arg_verbosity {
                0 => Level::WARN,
                1 => Level::INFO,
                2 => Level::DEBUG,
                _ => Level::TRACE,
            };

            let directive = if arg_verbosity == 0 {
                Directive::from_str(arg_directive)?
            } else {
                verbosity.into()
            };

            Ok::<_, anyhow::Error>(
                EnvFilter::builder()
                    .with_default_directive(directive)
                    .from_env()?,
            )
        };

        let mut layers = vec![];
        let mut non_blocking_guards = vec![];

        if !self.quiet {
            let filter = make_filter(self.verbosity, &self.log_stderr_directive)?;

            let (writer, guard) = tracing_appender::non_blocking(io::stderr());
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(!self.no_color)
                .with_file(false)
                .with_line_number(false)
                .with_span_events(FmtSpan::NONE)
                .with_target(false)
                .without_time()
                .with_filter(filter)
                .boxed();
            layers.push(layer);
            non_blocking_guards.push(guard);
        }

        #[cfg(unix)]
        if !self.no_log_journald {
            if let Ok(journald_layer) = tracing_journald::layer() {
                let filter = make_filter(self.verbosity, &self.log_journald_directive)?;
                layers.push(journald_layer.with_filter(filter).boxed());
            }
        }

        if !self.no_log_file {
            const MAX_FILES: usize = 25;
            const MAX_BYTES: usize = 10_000_000;

            let path = &self.log_file_directory;
            if !path.is_dir() {
                fs::create_dir_all(&path)?;
            }
            let basename = paths::log_basename(&path);
            let writer = FileRotate::new(
                basename,
                AppendTimestamp::default(FileLimit::MaxFiles(MAX_FILES)),
                ContentLimit::BytesSurpassed(MAX_BYTES),
                Compression::OnRotate(0),
                #[cfg(unix)]
                None,
            );
            let (writer, guard) = tracing_appender::non_blocking(writer);

            let filter = make_filter(self.verbosity, &self.log_file_directive)?;
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_file(true)
                .with_line_number(true)
                .with_target(true)
                .with_filter(filter)
                .boxed();
            layers.push(layer);
            non_blocking_guards.push(guard);
        }

        if layers.is_empty() {
            eprintln!("No logging is enabled. All logs will be lost.");
        }

        Ok((
            layers,
            Guard {
                _inner: non_blocking_guards,
            },
        ))
    }
}
