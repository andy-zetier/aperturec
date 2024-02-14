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
