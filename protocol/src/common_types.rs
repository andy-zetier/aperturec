include!(concat!(env!("OUT_DIR"), "/common_types.rs"));

#[cfg(test)]
mod test {
    use crate::common_types::*;
    use crate::test::*;

    #[test]
    fn serde_duration() {
        serde_type_der!(DurationMs, DurationMs(1337));
    }

    #[test]
    fn serde_client_id() {
        serde_type_der!(ClientId, ClientId(8908990));
    }

    #[test]
    fn serde_dimension() {
        let dimension = Dimension {
            width: 1920,
            height: 1080,
        };
        serde_type_der!(Dimension, dimension);
        c::round_trip_der!(Dimension, c::Dimension, dimension);
    }

    #[test]
    fn location() {
        let location = Location {
            x_position: 500,
            y_position: 250,
        };
        serde_type_der!(Location, location);
        c::round_trip_der!(Location, c::Location, location);
    }

    #[test]
    fn serde_codec() {
        serde_type_der!(Codec, Codec::Raw);
    }

    #[test]
    fn serde_heartbeat_id() {
        serde_type_der!(HeartbeatId, HeartbeatId(8092));
    }

    #[test]
    fn cursor_bitmap() {
        let cursor_bitmap = CursorBitmap {
            cursor: Cursor::Default,
            data: vec![].into(),
        };
        serde_type_der!(CursorBitmap, cursor_bitmap);
        c::round_trip_der!(CursorBitmap, c::CursorBitmap, cursor_bitmap);
    }
}
