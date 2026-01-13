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
