#![allow(clippy::too_many_arguments, clippy::large_enum_variant)]

pub mod convenience;

pub const MAGIC: &str = concat!(r"ApertureC-", env!("CARGO_PKG_VERSION_MAJOR"));

pub const LEGACY_ALPN: &str = "ApertureC-0.1.0";

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
