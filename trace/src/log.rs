//! Logging macros which integrate into the tracing system using the "log" target
use crate::Level;

use anyhow::Result;
use const_format::formatcp;
use parking_lot::{Mutex, MutexGuard};
use std::io::{self, Stderr, StderrLock, Stdout, StdoutLock, Write};

use tracing::level_filters::LevelFilter;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::filter::{filter_fn, EnvFilter};
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Default level if none is specified
pub const DEFAULT_LEVEL_FILTER: LevelFilter = LevelFilter::WARN;

/// Default directive passed via EnvFilter semantics
pub const DEFAULT_FILTER_DIRECTIVE: &str = formatcp!("{}=warn", TARGET);

/// Re-exported tracing module so queue macros expand properly at callsite
#[doc(hidden)]
pub use tracing;

/// Target value for all log tracing events
///
/// This value can be used with the
/// [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
/// to enable or disable log tracing
pub const TARGET: &str = "log";

// Nested macros with repetitions are currently broken
// (https://github.com/rust-lang/rust/issues/35853).  Instead of using some work-around, we just
// implement each macro for each level by hand.

/// Create a log at the trace level
#[doc(hidden)]
#[macro_export]
macro_rules! __trace__ {
    ( $fmt:literal $(, $args:expr )* ) => {
        $crate::log::tracing::trace!(target: $crate::log::TARGET, $fmt $(, $args )*);
    }
}
#[doc(inline)]
pub use __trace__ as trace;

/// Create a log at the debug level
#[doc(hidden)]
#[macro_export]
macro_rules! __debug__ {
    ( $fmt:literal $(, $args:expr )* ) => {
        $crate::log::tracing::debug!(target: $crate::log::TARGET, $fmt $(, $args )*);
    }
}
#[doc(inline)]
pub use __debug__ as debug;

/// Create a log at the info level
#[doc(hidden)]
#[macro_export]
macro_rules! __info__ {
    ( $fmt:literal $(, $args:expr )* ) => {
        $crate::log::tracing::info!(target: $crate::log::TARGET, $fmt $(, $args )*);
    }
}
#[doc(inline)]
pub use __info__ as info;

/// Create a log at the warn level
#[doc(hidden)]
#[macro_export]
macro_rules! __warn__ {
    ( $fmt:literal $(, $args:expr )* ) => {
        $crate::log::tracing::warn!(target: $crate::log::TARGET, $fmt $(, $args )*);
    }
}
#[doc(inline)]
pub use __warn__ as warn;

/// Create a log at the error level
#[doc(hidden)]
#[macro_export]
macro_rules! __error__ {
    ( $fmt:literal $(, $args:expr )* ) => {
        $crate::log::tracing::error!(target: $crate::log::TARGET, $fmt $(, $args )*);
    }
}
#[doc(inline)]
pub use __error__ as error;

struct StdioWriter {
    stdout: Stdout,
    stderr: Stderr,
}

impl Default for StdioWriter {
    fn default() -> Self {
        StdioWriter {
            stdout: io::stdout(),
            stderr: io::stderr(),
        }
    }
}

enum StdioLock<'l> {
    Stdout(StdoutLock<'l>),
    Stderr(StderrLock<'l>),
}

impl<'l> Write for StdioLock<'l> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            StdioLock::Stdout(lock) => lock.write(buf),
            StdioLock::Stderr(lock) => lock.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            StdioLock::Stdout(lock) => lock.flush(),
            StdioLock::Stderr(lock) => lock.flush(),
        }
    }
}

impl<'l> MakeWriter<'l> for StdioWriter {
    type Writer = StdioLock<'l>;

    fn make_writer(&'l self) -> Self::Writer {
        StdioLock::Stdout(self.stdout.lock())
    }

    fn make_writer_for(&'l self, meta: &Metadata<'_>) -> Self::Writer {
        if meta.level() <= &Level::WARN {
            StdioLock::Stderr(self.stderr.lock())
        } else {
            StdioLock::Stdout(self.stdout.lock())
        }
    }
}

pub(crate) fn custom_writer<W: Write + Send + 'static>(writer: W) -> BoxMakeWriter {
    BoxMakeWriter::new(CustomWriter::from(writer))
}

struct CustomWriter<W: Write> {
    inner: Mutex<W>,
}

impl<W: Write> From<W> for CustomWriter<W> {
    fn from(writer: W) -> Self {
        CustomWriter {
            inner: Mutex::new(writer),
        }
    }
}

impl<'l, W: Write + 'l> MakeWriter<'l> for CustomWriter<W> {
    type Writer = CustomWriterLock<'l, W>;

    fn make_writer(&'l self) -> Self::Writer {
        CustomWriterLock {
            lock_guard: self.inner.lock(),
        }
    }
}

struct CustomWriterLock<'l, W: Write + 'l> {
    lock_guard: MutexGuard<'l, W>,
}

impl<'l, W: Write + 'l> Write for CustomWriterLock<'l, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock_guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock_guard.flush()
    }
}

pub(crate) struct Configuration {
    pub(crate) writer: BoxMakeWriter,
    pub(crate) cmdline_verbosity: Option<Level>,
    pub(crate) log_to_file: bool,
    pub(crate) is_simple: bool,
}

impl Default for Configuration {
    fn default() -> Self {
        Configuration {
            writer: BoxMakeWriter::new(StdioWriter::default()),
            cmdline_verbosity: None,
            log_to_file: false,
            is_simple: false,
        }
    }
}

pub(crate) fn layer<'a, S>(
    common_config: &crate::CommonConfiguration<'a>,
    log_config: Configuration,
) -> Result<impl Layer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let mut writer = log_config.writer;
    if log_config.log_to_file {
        let output_directory = common_config.output_directory.join(TARGET);
        writer = BoxMakeWriter::new(writer.and(tracing_appender::rolling::daily(
            output_directory,
            common_config.component_name,
        )));
    }

    let mut log_env_filter = EnvFilter::builder().parse(common_config.trace_filter)?;
    if let Some(cmdline_verbosity) = log_config.cmdline_verbosity {
        log_env_filter =
            log_env_filter.add_directive(format!("{}={}", TARGET, cmdline_verbosity).parse()?);
    }

    let fmt = tracing_subscriber::fmt::format()
        .with_ansi(!log_config.is_simple && !cfg!(windows))
        .with_level(!log_config.is_simple)
        .with_source_location(!log_config.is_simple)
        .with_line_number(!log_config.is_simple)
        .with_target(false)
        .compact();
    Ok(tracing_subscriber::fmt::Layer::new()
        .event_format(fmt)
        .with_writer(writer)
        .with_filter(log_env_filter)
        .with_filter(filter_fn(|md| md.target() == TARGET)))
}
