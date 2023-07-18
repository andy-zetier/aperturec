pub mod common_types {
    include!(concat!(env!("OUT_DIR"), "/common_types.rs"));
}
pub mod control_messages {
    include!(concat!(env!("OUT_DIR"), "/control_messages.rs"));
}
pub mod event_messages {
    include!(concat!(env!("OUT_DIR"), "/event_messages.rs"));
}
pub mod media_messages {
    include!(concat!(env!("OUT_DIR"), "/media_messages.rs"));
}
