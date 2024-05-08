pub const MAGIC: &'static str = concat!(
    r"ApertureC-",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH")
);

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
