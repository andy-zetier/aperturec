//! Tracing facilities for ApertureC
use anyhow::Result;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub mod log;

#[cfg(feature = "queue")]
pub mod queue;

/// Re-export of tracing Level to prevent downstream crates from needing to directly depend on
/// tracing crate.
pub use tracing::Level;

/// Environment variable to set tracing filters at runtime
pub const FILTER_ENV_VAR: &str = "AC_TRACE_FILTER";

/// Environment variable to set output directory at runtime
pub const OUTDIR_ENV_VAR: &str = "AC_TRACE_OUTDIR";

/// Default directive used if none is supplied
const DEFAULT_FILTER_DIRECTIVES: &[&str] = &[
    log::DEFAULT_FILTER_DIRECTIVE,
    #[cfg(feature = "queue")]
    queue::DEFAULT_FILTER_DIRECTIVE,
];

static DEFAULT_FILTER_DIRECTIVE: OnceLock<String> = OnceLock::new();

pub fn default_filter_directive() -> &'static str {
    DEFAULT_FILTER_DIRECTIVE.get_or_init(|| DEFAULT_FILTER_DIRECTIVES.join(","))
}

/// Default output directory placed in $TMPDIR if none is specified
const DEFAULT_OUTPUT_DIRECTORY: &str = "aperturec-trace";

struct CommonConfiguration<'a> {
    component_name: &'a str,
    output_directory: &'a Path,
    trace_filter: &'a str,
}

impl<'a> CommonConfiguration<'a> {
    fn new(component_name: &'a str) -> Self {
        CommonConfiguration {
            component_name,
            output_directory: default_output_path(),
            trace_filter: default_filter_directive(),
        }
    }
}

/// Configure the tracing system
pub struct Configuration<'a> {
    common: CommonConfiguration<'a>,
    log: log::Configuration,
}

impl<'a> Configuration<'a> {
    /// Create a new tracing configuration with the given component name
    ///
    /// The component name may be used by the tracing system for output purposes, e.g. naming log
    /// files.
    pub fn new(component_name: &'a str) -> Self {
        Configuration {
            common: CommonConfiguration::new(component_name),
            log: log::Configuration::default(),
        }
    }
}

impl<'a> Configuration<'a> {
    /// Set verbosity for logging that overrides any filter directive
    pub fn cmdline_verbosity(mut self, v: Level) -> Self {
        self.log.cmdline_verbosity = Some(v);
        self
    }

    /// Set the root output directory for tracing data
    pub fn output_directory(mut self, output_directory: &'a Path) -> Self {
        self.common.output_directory = output_directory;
        self
    }

    /// Set a trace filter string
    pub fn trace_filter(mut self, filter: &'a str) -> Self {
        self.common.trace_filter = filter;
        self
    }

    /// Set a custom writer for logging
    pub fn writer<W: Write + Send + 'static>(mut self, writer: W) -> Self {
        self.log.writer = log::custom_writer(writer);
        self
    }

    /// Strip all log fanciness from output including ANSI sequences, source file, etc.
    pub fn simple(mut self, is_simple: bool) -> Self {
        self.log.is_simple = is_simple;
        self
    }

    /// Log all output to rotating files in the trace output directory in addition to the custom
    /// writer or stdout/stderr if no custom writer is supplied
    pub fn log_to_file(mut self, log_to_file: bool) -> Self {
        self.log.log_to_file = log_to_file;
        self
    }

    /// Initialize the tracing system with the configuration
    pub fn initialize(self) -> Result<()> {
        #[cfg(feature = "queue")]
        {
            tracing_subscriber::registry()
                .with(log::layer(&self.common, self.log)?)
                .with(queue::layer(&self.common)?)
                .try_init()?;
        }
        #[cfg(not(feature = "queue"))]
        {
            tracing_subscriber::registry()
                .with(log::layer(&self.common, self.log)?)
                .try_init()?;
        }
        Ok(())
    }
}

static DEFAULT_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Get the default output path
pub fn default_output_path() -> &'static Path {
    DEFAULT_PATH
        .get_or_init(|| {
            PathBuf::from(env::var("TMPDIR").unwrap_or("/tmp".to_string()))
                .join(DEFAULT_OUTPUT_DIRECTORY)
        })
        .as_path()
}

/// Get the default output path as a string
pub fn default_output_path_display() -> &'static str {
    default_output_path()
        .to_str()
        .expect("retrieve default output trace path")
}
