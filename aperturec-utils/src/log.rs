use tracing::*;
use tracing_subscriber::{
    layer::{Context, Filter},
    registry::LookupSpan,
};

pub const ALWAYS_TARGET_NAME: &str = "ALWAYS";
pub struct Always;

impl<S> Filter<S> for Always
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        meta.target() == ALWAYS_TARGET_NAME
    }
}

#[macro_export(local_inner_macros)]
macro_rules! event_always {
    ($level:expr, $($input:tt)*) => {{
        tracing::event!(target: $crate::log::ALWAYS_TARGET_NAME, $level, $($input)*);
    }}
}
pub use event_always;

#[macro_export(local_inner_macros)]
macro_rules! trace_always {
    ($($input:tt)*) => {{
        event_always!(::tracing::Level::TRACE, $($input)*)
    }}
}
pub use trace_always;

#[macro_export(local_inner_macros)]
macro_rules! debug_always {
    ($($input:tt)*) => {{
        event_always!(::tracing::Level::DEBUG, $($input)*)
    }}
}
pub use debug_always;

#[macro_export(local_inner_macros)]
macro_rules! info_always {
    ($($input:tt)*) => {{
        event_always!(::tracing::Level::INFO, $($input)*)
    }}
}
pub use info_always;

#[macro_export(local_inner_macros)]
macro_rules! warn_always {
    ($($input:tt)*) => {{
        event_always!(::tracing::Level::WARN, $($input)*)
    }}
}
pub use warn_always;

#[macro_export(local_inner_macros)]
macro_rules! error_always {
    ($($input:tt)*) => {{
        event_always!(::tracing::Level::ERROR, $($input)*)
    }}
}
pub use error_always;

#[macro_export(local_inner_macros)]
macro_rules! log_early {
    ($level:expr, $($input:tt)*) => {{
        std::eprint!("{:>5} ", $level);
        std::eprintln!($($input)*);
    }}
}
pub use log_early;

#[macro_export(local_inner_macros)]
macro_rules! trace_early {
    ($($input:tt)*) => {{
        $crate::log::log_early!(::tracing::Level::TRACE, $($input)*);
    }}
}
pub use trace_early;

#[macro_export(local_inner_macros)]
macro_rules! debug_early {
    ($($input:tt)*) => {{
        $crate::log::log_early!(::tracing::Level::DEBUG, $($input)*);
    }}
}
pub use debug_early;

#[macro_export(local_inner_macros)]
macro_rules! info_early {
    ($($input:tt)*) => {{
        $crate::log::log_early!(::tracing::Level::INFO, $($input)*);
    }}
}
pub use info_early;

#[macro_export(local_inner_macros)]
macro_rules! warn_early {
    ($($input:tt)*) => {{
        $crate::log::log_early!(::tracing::Level::WARN, $($input)*);
    }}
}
pub use warn_early;

#[macro_export(local_inner_macros)]
macro_rules! error_early {
    ($($input:tt)*) => {{
        $crate::log::log_early!(::tracing::Level::ERROR, $($input)*);
    }}
}
pub use error_early;
