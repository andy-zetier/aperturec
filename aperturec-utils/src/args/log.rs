use crate::{log, paths};

use anyhow::Result;
use file_rotate::{
    compression::Compression,
    suffix::{AppendTimestamp, FileLimit},
    ContentLimit, FileRotate,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::*;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    filter::{Directive, FilterExt},
    fmt::format::FmtSpan,
    layer::Filter,
    registry::LookupSpan,
    *,
};

#[derive(Debug, clap::Args)]
pub struct LogArgGroup {
    /// Log level verbosity for all log outputs. Multiple -v options increase the verbosity. The
    /// maximum is 2. This can be overridden using the RUST_LOG environment variable. Overrides all
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
    #[arg(long, conflicts_with = "quiet", default_value = "info")]
    log_stderr_directive: String,

    /// Disable logging to journald
    #[arg(long, action = clap::ArgAction::SetTrue, default_value = "false")]
    #[cfg(target_os = "linux")]
    no_log_journald: bool,

    /// journald logging directive. Follows the format of RUST_LOG/EnvFilter
    #[arg(long, conflicts_with = "no_log_journald", default_value = "info")]
    #[cfg(target_os = "linux")]
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

fn create_filter<S>(verbosity: u8, directive: &str) -> Result<impl Filter<S>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let level = match verbosity {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    let directive = if verbosity == 0 {
        Directive::from_str(directive)?
    } else {
        level.into()
    };

    Ok(EnvFilter::builder()
        .with_default_directive(directive)
        .from_env()?
        .or(log::Always))
}

fn create_stderr_layer<S>(
    disabled: bool,
    verbosity: u8,
    directive: &str,
    color: bool,
) -> Result<(impl Layer<S>, WorkerGuard)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let filter = if !disabled {
        create_filter(verbosity, directive)?.boxed()
    } else {
        log::Always.boxed()
    };

    let (writer, guard) = tracing_appender::non_blocking(io::stderr());
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(color)
        .with_file(false)
        .with_line_number(false)
        .with_span_events(FmtSpan::NONE)
        .with_target(false)
        .without_time()
        .with_filter(filter);
    Ok((layer, guard))
}

#[cfg(target_os = "linux")]
fn create_journald_layer<S>(verbosity: u8, directive: &str) -> Result<impl Layer<S>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let journald_layer = tracing_journald::layer()?;
    let filter = create_filter(verbosity, directive)?;
    Ok(journald_layer.with_filter(filter))
}

fn create_file_layer<S>(
    verbosity: u8,
    directive: &str,
    file_directory: &Path,
) -> Result<(impl Layer<S>, WorkerGuard)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    const MAX_FILES: usize = 25;
    const MAX_BYTES: usize = 10_000_000;

    let path = &file_directory;
    if !path.is_dir() {
        fs::create_dir_all(path)?;
    }
    let basename = paths::log_basename(path);
    let writer = FileRotate::new(
        basename,
        AppendTimestamp::default(FileLimit::MaxFiles(MAX_FILES)),
        ContentLimit::BytesSurpassed(MAX_BYTES),
        Compression::OnRotate(0),
        #[cfg(unix)]
        None,
    );
    let (writer, guard) = tracing_appender::non_blocking(writer);

    let filter = create_filter(verbosity, directive)?;
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_target(true)
        .with_filter(filter);
    Ok((layer, guard))
}

pub struct Guard {
    _inner: Vec<WorkerGuard>,
}

impl LogArgGroup {
    pub fn as_tracing_layer<S>(&self) -> Result<(impl Layer<S> + Send + Sync, Guard)>
    where
        S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        let (stderr_layer, stderr_guard) = create_stderr_layer(
            self.quiet,
            self.verbosity,
            &self.log_stderr_directive,
            !self.no_color,
        )?;
        let mut layers = vec![stderr_layer.boxed()];
        let mut non_blocking_guards = vec![stderr_guard];

        if !self.no_log_file {
            let (file_layer, file_guard) = create_file_layer(
                self.verbosity,
                &self.log_file_directive,
                &self.log_file_directory,
            )?;
            layers.push(file_layer.boxed());
            non_blocking_guards.push(file_guard);
        }

        #[cfg(target_os = "linux")]
        if !self.no_log_journald {
            match create_journald_layer(self.verbosity, &self.log_journald_directive) {
                Ok(layer) => layers.push(layer.boxed()),
                Err(e) => log::warn_early!("Failed setting up journald logging: {}", e),
            }
        }

        #[cfg(target_os = "linux")]
        let no_log_journald = self.no_log_journald;
        #[cfg(not(target_os = "linux"))]
        let no_log_journald = true;

        if self.quiet && no_log_journald && self.no_log_file {
            log::warn_early!("No logging is enabled. All logs will be lost.");
        }

        Ok((
            layers,
            Guard {
                _inner: non_blocking_guards,
            },
        ))
    }
}
