use std::env;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};

use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Copy, Clone, Debug)]
pub enum UserLevel {
    Info,
    Warn,
    Error,
}

pub const USER_OUTPUT_TARGET: &str = "user_output";
static USER_OUTPUT_STREAM: OnceLock<Mutex<StandardStream>> = OnceLock::new();

#[doc(hidden)]
pub fn user_output_write(level: UserLevel, args: fmt::Arguments) -> io::Result<()> {
    let mut out = USER_OUTPUT_STREAM
        .get_or_init(|| {
            let no_color = env::var_os("NO_COLOR").is_some() || !io::stdout().is_terminal();
            let choice = if no_color {
                ColorChoice::Never
            } else {
                ColorChoice::Auto
            };
            Mutex::new(StandardStream::stdout(choice))
        })
        .lock()
        .map_err(|_| io::Error::other("user output stream lock poisoned"))?;
    let mut spec = ColorSpec::new();
    spec.set_bold(false);
    match level {
        UserLevel::Info => spec.set_fg(Some(Color::Green)),
        UserLevel::Warn => spec.set_fg(Some(Color::Yellow)),
        UserLevel::Error => spec.set_fg(Some(Color::Red)),
    };
    out.set_color(&spec)?;
    let label = match level {
        UserLevel::Info => "INFO",
        UserLevel::Warn => "WARN",
        UserLevel::Error => "ERROR",
    };
    write!(out, "{:>5} ", label)?;
    out.reset()?;
    out.write_fmt(args)?;
    out.write_all(b"\n")?;
    Ok(())
}

#[doc(hidden)]
pub fn user_output_trace(level: UserLevel, args: fmt::Arguments) {
    match level {
        UserLevel::Info => ::tracing::event!(
            target: USER_OUTPUT_TARGET,
            ::tracing::Level::INFO,
            "{}",
            args
        ),
        UserLevel::Warn => ::tracing::event!(
            target: USER_OUTPUT_TARGET,
            ::tracing::Level::WARN,
            "{}",
            args
        ),
        UserLevel::Error => ::tracing::event!(
            target: USER_OUTPUT_TARGET,
            ::tracing::Level::ERROR,
            "{}",
            args
        ),
    }
}

#[macro_export(local_inner_macros)]
/// Emit a user-facing message to stdout, optionally also to tracing.
///
/// By default, this writes to stdout and also forwards to tracing at the
/// corresponding level. You can disable tracing by passing `trace = false`.
///
/// # Examples
/// ```
/// use aperturec_utils::user_output;
///
/// let addr = "127.0.0.1";
/// user_output!(Info, "Connected to {}", addr);
/// user_output!(Warn, trace = false, "Retrying...");
/// ```
macro_rules! user_output {
    (level = $level:expr, trace = $trace:expr, $($input:tt)*) => {{
        let args = ::std::format_args!($($input)*);
        let level = $level;
        if let Err(error) = $crate::user_output::user_output_write(level, args) {
            ::tracing::warn!(%error, "Failed to write user output");
        }
        if $trace {
            $crate::user_output::user_output_trace(level, args);
        }
    }};
    ($level:ident, trace = $trace:expr, $($input:tt)*) => {{
        $crate::user_output!(
            level = $crate::user_output::UserLevel::$level,
            trace = $trace,
            $($input)*
        );
    }};
    ($level:ident, $($input:tt)*) => {{
        $crate::user_output!(level = $crate::user_output::UserLevel::$level, $($input)*);
    }};
    (level = $level:expr, $($input:tt)*) => {{
        $crate::user_output!(level = $level, trace = true, $($input)*);
    }}
}
pub use user_output;

#[macro_export(local_inner_macros)]
/// Emit an INFO user-facing message to stdout, optionally also to tracing.
///
/// By default, this writes to stdout and also forwards to tracing. You can
/// disable tracing by passing `trace = false`.
///
/// # Examples
/// ```
/// use aperturec_utils::user_info;
///
/// let addr = "127.0.0.1";
/// user_info!("Connected to {}", addr);
/// user_info!(trace = false, "Connected to {}", addr);
/// ```
macro_rules! user_info {
    (trace = $trace:expr, $($input:tt)*) => {{
        $crate::user_output!(Info, trace = $trace, $($input)*);
    }};
    ($($input:tt)*) => {{
        $crate::user_output!(Info, $($input)*);
    }};
}
pub use user_info;

#[macro_export(local_inner_macros)]
/// Emit a WARN user-facing message to stdout, optionally also to tracing.
///
/// By default, this writes to stdout and also forwards to tracing. You can
/// disable tracing by passing `trace = false`.
///
/// # Examples
/// ```
/// use aperturec_utils::user_warn;
///
/// user_warn!("Retrying...");
/// user_warn!(trace = false, "Retrying...");
/// ```
macro_rules! user_warn {
    (trace = $trace:expr, $($input:tt)*) => {{
        $crate::user_output!(Warn, trace = $trace, $($input)*);
    }};
    ($($input:tt)*) => {{
        $crate::user_output!(Warn, $($input)*);
    }};
}
pub use user_warn;

#[macro_export(local_inner_macros)]
/// Emit an ERROR user-facing message to stdout, optionally also to tracing.
///
/// By default, this writes to stdout and also forwards to tracing. You can
/// disable tracing by passing `trace = false`.
///
/// # Examples
/// ```
/// use aperturec_utils::user_error;
///
/// user_error!("Connection failed");
/// user_error!(trace = false, "Connection failed");
/// ```
macro_rules! user_error {
    (trace = $trace:expr, $($input:tt)*) => {{
        $crate::user_output!(Error, trace = $trace, $($input)*);
    }};
    ($($input:tt)*) => {{
        $crate::user_output!(Error, $($input)*);
    }};
}
pub use user_error;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn user_output_interleaving() {
        let threads = 8usize;
        let lines_per_thread = 500usize;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..lines_per_thread {
                    // Run with -- --nocapture to inspect for interleaving.
                    crate::user_info!("tag=t{} line={}", t, i);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("child thread panicked");
        }
    }
}
