#![allow(clippy::too_many_arguments, clippy::large_enum_variant)]

use std::fmt;

pub mod convenience;

pub const MAGIC: &str = concat!(r"ApertureC-", env!("CARGO_PKG_VERSION_MAJOR"));

pub const LEGACY_ALPN: &str = "ApertureC-0.1.0";

/// Error when converting a struct which wraps a single Option to its inner type, and
/// the Option is None.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrappedOptionalConvertError;

impl fmt::Display for WrappedOptionalConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unwrapping expected optional field in protocol")
    }
}

impl std::error::Error for WrappedOptionalConvertError {}

pub mod common {
    include!(concat!(env!("OUT_DIR"), "/common.rs"));
}

pub mod control {
    include!(concat!(env!("OUT_DIR"), "/control.rs"));
}

pub mod event {
    include!(concat!(env!("OUT_DIR"), "/event.rs"));
}

pub mod media {
    include!(concat!(env!("OUT_DIR"), "/media.rs"));
}

pub mod tunnel {
    include!(concat!(env!("OUT_DIR"), "/tunnel.rs"));
}
