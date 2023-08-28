include!(concat!(env!("OUT_DIR"), "/media_messages.rs"));

#[cfg(test)]
mod test {
    use crate::media_messages::*;
    use crate::test::*;

    #[test]
    fn rectangle() {
        let rect = Rectangle {
            codec: Codec::Raw,
            data: vec![1, 2, 3, 4, 5].into(),
            dimension: None,
        };
        serde_type_der!(Rectangle, rect);
        c::round_trip_der!(Rectangle, c::Rectangle, rect);
    }

    #[test]
    fn rectangle_update() {
        let ru = RectangleUpdate {
            sequence_id: SequenceId(12),
            location: Location {
                x_position: 300,
                y_position: 600,
            },
            rectangle: Rectangle {
                codec: Codec::Avif,
                data: vec![180, 10, 220].into(),
                dimension: Some(Dimension {
                    width: 800,
                    height: 600,
                }),
            },
        };
        serde_type_der!(RectangleUpdate, ru);
        c::round_trip_der!(RectangleUpdate, c::RectangleUpdate, ru);
    }

    #[test]
    fn frame_buffer_update() {
        let fbu = FramebufferUpdate {
            rectangle_updates: vec![
                RectangleUpdate {
                    sequence_id: SequenceId(12),
                    location: Location {
                        x_position: 20,
                        y_position: 40,
                    },
                    rectangle: Rectangle {
                        codec: Codec::Avif,
                        data: vec![].into(),
                        dimension: None,
                    },
                },
                RectangleUpdate {
                    sequence_id: SequenceId(12),
                    location: Location {
                        x_position: 80,
                        y_position: 30,
                    },
                    rectangle: Rectangle {
                        codec: Codec::Avif,
                        data: vec![89, 123, 41, 30, 60, 91].into(),
                        dimension: Some(Dimension {
                            width: 1,
                            height: 2,
                        }),
                    },
                },
            ],
        };
        serde_type_der!(FramebufferUpdate, fbu);
        c::round_trip_der!(FramebufferUpdate, c::FramebufferUpdate, fbu);
    }

    #[test]
    fn client_to_server_message() {
        let msg = ServerToClientMessage::FramebufferUpdate(FramebufferUpdate {
            rectangle_updates: vec![],
        });
        serde_type_der!(ServerToClientMessage, msg);
        c::round_trip_der!(
            ServerToClientMessage,
            c::MediaMessages_ServerToClientMessage,
            msg
        );
    }
}
