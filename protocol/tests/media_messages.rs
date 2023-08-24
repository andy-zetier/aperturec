extern crate aperturec_protocol;
extern crate asn1rs;

use aperturec_protocol::common_types::*;
use aperturec_protocol::media_messages::*;
use asn1rs::prelude::*;

#[macro_use]
mod macros;

#[test]
fn serde_rectangle() {
    serde_type!(
        Rectangle,
        Rectangle {
            codec: Codec::Raw,
            data: vec![1, 2, 3, 4, 5],
            dimension: None
        }
    )
}

#[test]
fn serde_rectangle_update() {
    serde_type!(
        RectangleUpdate,
        RectangleUpdate {
            sequence_id: SequenceId(12),
            location: Location {
                x_position: 300,
                y_position: 600
            },
            rectangle: Rectangle {
                codec: Codec::Avif,
                data: vec![180, 10, 220],
                dimension: Some(Dimension {
                    width: 800,
                    height: 600
                })
            }
        }
    );
}

#[test]
fn serde_frame_buffer_update() {
    serde_type!(
        FramebufferUpdate,
        FramebufferUpdate {
            rectangle_updates: vec![
                RectangleUpdate {
                    sequence_id: SequenceId(12),
                    location: Location {
                        x_position: 20,
                        y_position: 40,
                    },
                    rectangle: Rectangle {
                        codec: Codec::Avif,
                        data: vec![],
                        dimension: None
                    }
                },
                RectangleUpdate {
                    sequence_id: SequenceId(12),
                    location: Location {
                        x_position: 80,
                        y_position: 30
                    },
                    rectangle: Rectangle {
                        codec: Codec::Avif,
                        data: vec![89, 123, 41, 30, 60, 91],
                        dimension: Some(Dimension {
                            width: 1,
                            height: 2,
                        })
                    }
                },
            ],
        }
    );
}

#[test]
fn serde_client_to_server_message() {
    serde_type!(
        ServerToClientMessage,
        ServerToClientMessage::FramebufferUpdate(FramebufferUpdate {
            rectangle_updates: vec![]
        })
    );
}
