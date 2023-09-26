include!(concat!(env!("OUT_DIR"), "/event_messages.rs"));

#[cfg(test)]
mod test {
    use crate::event_messages::*;
    use crate::test::*;

    #[test]
    fn button_state() {
        let bs = ButtonState {
            button: Button::MappedButton(MappedButton::Left),
            is_depressed: false,
        };
        serde_type_der!(ButtonState, bs);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(ButtonState, c::ButtonState, bs);
    }

    #[test]
    fn key_event() {
        let ke = KeyEvent {
            down: true,
            key: 4096,
        };
        serde_type_der!(KeyEvent, ke);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(KeyEvent, c::KeyEvent, ke);
    }

    #[test]
    fn pointer_event() {
        let pe = PointerEvent {
            button_states: vec![ButtonState {
                button: Button::UnmappedButton(10),
                is_depressed: true,
            }],
            location: Location {
                x_position: 0,
                y_position: 0,
            },
            cursor: Cursor::Wait,
        };
        serde_type_der!(PointerEvent, pe);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(PointerEvent, c::PointerEvent, pe);
    }

    #[test]
    fn display_event() {
        let de = DisplayEvent {
            display_size: Dimension {
                width: 1024,
                height: 768,
            },
        };
        serde_type_der!(DisplayEvent, de);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(DisplayEvent, c::DisplayEvent, de);
    }

    #[test]
    fn client_to_server_message() {
        let msg = ClientToServerMessage::KeyEvent(KeyEvent {
            down: false,
            key: 0,
        });
        serde_type_der!(ClientToServerMessage, msg);
        #[cfg(feature = "asn1c-tests")]
        c::round_trip_der!(
            ClientToServerMessage,
            c::EventMessages_ClientToServerMessage,
            msg
        );
    }
}
