extern crate aperturec_protocol;
extern crate asn1rs;

use aperturec_protocol::common_types::*;
use aperturec_protocol::event_messages::*;
use asn1rs::prelude::*;

#[macro_use]
mod macros;

#[test]
fn serde_button_state() {
    serde_type!(
        ButtonState,
        ButtonState {
            button: Button::MappedButton(MappedButton::Left),
            is_depressed: false
        }
    );
}

#[test]
fn serde_key_event() {
    serde_type!(
        KeyEvent,
        KeyEvent {
            down: true,
            key: 4096
        }
    );
}

#[test]
fn serde_pointer_event() {
    serde_type!(
        PointerEvent,
        PointerEvent {
            button_states: vec![ButtonState {
                button: Button::UnmappedButton(10),
                is_depressed: true
            }],
            location: Location {
                x_position: 0,
                y_position: 0
            },
            cursor: Cursor::Wait
        }
    );
}

#[test]
fn serde_display_event() {
    serde_type!(
        DisplayEvent,
        DisplayEvent {
            display_size: Dimension {
                width: 1024,
                height: 768
            }
        }
    );
}

#[test]
fn serde_client_to_server_message() {
    serde_type!(
        ClientToServerMessage,
        ClientToServerMessage::KeyEvent(KeyEvent {
            down: false,
            key: 0
        })
    );
}
