use crate::metrics::EventChannelSendLatency;

use aperturec_channel::{self as channel, Receiver as _, Sender as _};
use aperturec_graphics::display::DisplayConfiguration;
use aperturec_graphics::prelude::{Pixel32, Point};
use aperturec_metrics::time;
use aperturec_protocol::{
    common::{Cursor as ProtoCursor, DisplayInfo, Location},
    event::{
        self as proto, Button, MappedButton, client_to_server as c2s, server_to_client as s2c,
    },
};
use aperturec_utils::channels::SenderExt;

use crossbeam::channel::{Receiver, Sender, bounded, select_biased};
use ndarray::ArcArray2;
use std::{collections::BTreeMap, mem, thread};
use tracing::*;

/// Notifications the event channel can send back to the primary thread.
#[derive(Debug, derive_more::From)]
pub enum PrimaryThreadNotification {
    /// Server sent an updated display configuration that the UI should apply.
    DisplayConfiguration(DisplayConfiguration),
    /// Server requested the client change the cursor image.
    Cursor(Cursor),
    /// Event channel hit a fatal error and needs to tear down the session.
    Error(Error),
}
type Ptn = PrimaryThreadNotification;

/// Helper trait for mapping platform/event-loop button codes into protocol messages.
trait FromCode {
    type Code;
    /// Convert a platform-specific button identifier into a protocol button.
    fn from_code(code: Self::Code) -> Self;
}

impl FromCode for Button {
    type Code = u32;

    fn from_code(button: Self::Code) -> Self {
        let kind = Some(match MappedButton::try_from(button as i32 + 1) {
            Ok(mapped) => proto::button::Kind::MappedButton(mapped.into()),
            Err(_) => proto::button::Kind::UnmappedButton(button),
        });
        Button { kind }
    }
}

impl From<UserEvent> for c2s::Message {
    fn from(notif: UserEvent) -> Self {
        match notif {
            UserEvent::MouseButton {
                code,
                is_pressed,
                pos,
            } => proto::PointerEventBuilder::default()
                .button_states(vec![
                    proto::ButtonStateBuilder::default()
                        .button(Button::from_code(code))
                        .is_depressed(is_pressed)
                        .build()
                        .expect("Failed to build ButtonState"),
                ])
                .location(Location::try_from(pos).expect("convert point to location"))
                .cursor(ProtoCursor::Default)
                .build()
                .expect("Failed to build PointerEvent")
                .into(),
            UserEvent::Pointer { pos } => proto::PointerEventBuilder::default()
                .button_states(vec![])
                .location(Location::try_from(pos).expect("convert point to location"))
                .cursor(ProtoCursor::Default)
                .build()
                .expect("Failed to build PointerEvent")
                .into(),
            UserEvent::Key { code, is_pressed } => proto::KeyEvent {
                key: code,
                down: is_pressed,
            }
            .into(),
            UserEvent::DisplayChange { displays } => proto::DisplayEvent {
                displays: displays
                    .iter()
                    .map(|di| DisplayInfo {
                        area: Some(
                            di.area
                                .try_into()
                                .expect("Failed to convert Rect to Rectangle"),
                        ),
                        is_enabled: di.is_enabled,
                    })
                    .collect(),
            }
            .into(),
        }
    }
}

/// Client-side input notifications that eventually become protocol messages.
#[derive(Debug)]
pub enum UserEvent {
    /// Mouse button state change at a specific position.
    MouseButton {
        code: u32,
        is_pressed: bool,
        pos: Point,
    },
    /// Mouse pointer moved without a button change.
    Pointer { pos: Point },
    /// Keyboard key pressed or released.
    Key { code: u32, is_pressed: bool },
    /// Local display configuration changed.
    DisplayChange {
        displays: Vec<aperturec_graphics::display::Display>,
    },
}

#[derive(Debug)]
pub enum Notification {
    /// User-generated input to forward to the server.
    UserEvent(UserEvent),
    /// Ask the event channel threads to shut down.
    Terminate,
}

/// Cursor appearance and hotspot information.
///
/// Represents a cursor image with its pixel data and hotspot position.
/// The hotspot is the point within the cursor image that corresponds to
/// the actual pointer position.
#[derive(Clone, Debug)]
pub struct Cursor {
    /// Hotspot position within the cursor image.
    ///
    /// This is the point that corresponds to the actual mouse pointer position.
    pub hot: Point,
    /// Cursor pixel data in 32-bit RGBA format.
    ///
    /// The cursor image is stored as a 2D array of RGBA pixels.
    pub pixels: ArcArray2<Pixel32>,
}

impl TryFrom<proto::CursorImage> for Cursor {
    type Error = Error;

    fn try_from(ci: proto::CursorImage) -> Result<Self> {
        let width: usize = ci
            .width
            .try_into()
            .map_err(|e| Error::Protocol(Box::new(e)))?;
        let height: usize = ci
            .height
            .try_into()
            .map_err(|e| Error::Protocol(Box::new(e)))?;
        if width * height * mem::size_of::<Pixel32>() != ci.data.len() {
            return Err(Error::Protocol(
                format!(
                    "pixels buffer of length {} does not match expected size {} bytes for dimensions {width}x{height}",
                    ci.data.len(),
                    width * height * mem::size_of::<Pixel32>()
                )
                .into(),
            ));
        }
        let hot = Point::new(
            ci.x_hot
                .try_into()
                .map_err(|e| Error::Protocol(Box::new(e)))?,
            ci.y_hot
                .try_into()
                .map_err(|e| Error::Protocol(Box::new(e)))?,
        );

        let pixels = ci
            .data
            .as_chunks::<{ mem::size_of::<Pixel32>() }>()
            .0
            .iter()
            .copied()
            .map(Pixel32::from)
            .collect::<Vec<_>>();
        Ok(Cursor {
            hot,
            pixels: ArcArray2::from_shape_vec((height, width), pixels)
                .map_err(|e| Error::Protocol(Box::new(e)))?,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("channel receive")]
    ChannelRx(#[from] channel::codec::in_order::RxError),
    #[error("channel send")]
    ChannelTx(#[from] channel::codec::in_order::TxError),
    #[error(transparent)]
    Protocol(Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Spawn event-channel workers to bridge user input and server event messages.
///
/// A network reader turns server messages (display config, cursor images/changes) into
/// `PrimaryThreadNotification`s, while the main loop forwards user input to the server and
/// maintains a cursor cache so future `CursorChange` messages can be resolved locally.
///
/// # Parameters
/// * `client_ec` - Event channel handle split into Rx/Tx halves.
/// * `pt_tx` - Sends display/cursor updates or errors to the primary thread.
/// * `from_pt_rx` - Receives user input notifications or termination requests.
pub fn setup(
    client_ec: channel::ClientEvent,
    pt_tx: Sender<Ptn>,
    from_pt_rx: Receiver<Notification>,
) {
    let (mut ec_rx, mut ec_tx) = client_ec.split();

    let (from_network_tx, from_network_rx) = bounded(0);

    let network_rx_thread = thread::spawn(move || {
        let _s = debug_span!("ec-network-rx").entered();
        debug!("started");
        loop {
            match ec_rx.receive() {
                Ok(msg) => from_network_tx.send_or_warn(Ok(msg)),
                Err(err) => {
                    from_network_tx.send_or_warn(Err(err));
                    break;
                }
            }
        }
        debug!("exiting");
    });

    let mut cursor_cache = BTreeMap::new();
    thread::spawn(move || {
        let _s = debug_span!("ec-main").entered();
        debug!("started");
        loop {
            select_biased! {
                recv(from_pt_rx) -> pt_msg_res => {
                    let Ok(pt_msg) = pt_msg_res else {
                        warn!("primary died before ec-main");
                        break;
                    };
                    let ue = match pt_msg {
                        Notification::Terminate => break,
                        Notification::UserEvent(ue) => ue,
                    };
                    if matches!(ue, UserEvent::Pointer { .. }) {
                        trace!(?ue);
                    } else {
                        debug!(?ue);
                    }

                    let send_res = time!(EventChannelSendLatency, ec_tx.send(ue.into()));
                    if let Err(error) = send_res {
                        error!(%error);
                        pt_tx.send_or_warn(Ptn::Error(error.into()));
                    }
                }
                recv(from_network_rx) -> network_rx_res => {
                    let Ok(network_msg) = network_rx_res else {
                        debug!("ec-network-rx died before ec-main");
                        break;
                    };
                    match network_msg {
                        Ok(s2c::Message::DisplayConfiguration(display_config)) => {
                            trace!(?display_config);
                            let display_config = match DisplayConfiguration::try_from(display_config) {
                                Ok(dc) => dc,
                                Err(error) => {
                                    warn!(?error, "invalid display configuration");
                                    continue;
                                }
                            };
                            pt_tx.send_or_warn(Ptn::from(display_config));
                        }
                        Ok(s2c::Message::CursorImage(cursor_image)) => {
                            trace!(?cursor_image);
                            let id = cursor_image.id;
                            let cursor = match Cursor::try_from(cursor_image) {
                                Ok(cursor) => cursor,
                                Err(error) => {
                                    warn!(%id, %error);
                                    continue;
                                }
                            };

                            if let Some(existing) = cursor_cache.insert(id, cursor) {
                                warn!(%id, ?existing, "replaced existing cursor");
                            }
                        }
                        Ok(s2c::Message::CursorChange(cursor_change)) => {
                            trace!(?cursor_change);
                            if let Some(cursor) = cursor_cache.get(&cursor_change.id) {
                                pt_tx.send_or_warn(Ptn::Cursor(cursor.clone()));
                            } else {
                                warn!("received cursor change notification for missing cursor");
                            }
                        }
                        Err(err) => {
                            pt_tx.send_or_warn(Ptn::Error(err.into()));
                        }
                    }
                }
            }
        }

        if let Err(error) = network_rx_thread.join() {
            warn!("ec-network-rx panicked: {:?}", error)
        }
        debug!("exiting");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aperturec_graphics::{
        display::Display,
        geometry::{Rect, Size},
    };
    use aperturec_protocol::common::Rectangle;
    use std::convert::{TryFrom, TryInto};

    fn mapped_from_code(code: u32) -> MappedButton {
        let button = Button::from_code(code);
        match button.kind {
            Some(proto::button::Kind::MappedButton(value)) => {
                MappedButton::try_from(value).expect("valid mapped button value")
            }
            other => panic!("expected mapped button, got {other:?}"),
        }
    }

    #[test]
    fn from_code_maps_zero_to_left_button() {
        assert_eq!(mapped_from_code(0), MappedButton::Left);
    }

    #[test]
    fn from_code_maps_highest_known_button() {
        assert_eq!(mapped_from_code(8), MappedButton::Forward);
    }

    #[test]
    fn from_code_emits_unmapped_for_unknown_codes() {
        let code = 42;
        let button = Button::from_code(code);
        match button.kind {
            Some(proto::button::Kind::UnmappedButton(raw)) => assert_eq!(raw, code),
            other => panic!("expected unmapped button, got {other:?}"),
        }
    }

    #[test]
    fn notification_mouse_button_to_pointer_event() {
        let notif = UserEvent::MouseButton {
            code: 0,
            is_pressed: true,
            pos: Point::new(10, 20),
        };
        let message = c2s::Message::from(notif);
        let pointer = match message {
            c2s::Message::PointerEvent(pe) => pe,
            other => panic!("expected pointer event, got {other:?}"),
        };

        assert_eq!(pointer.button_states.len(), 1);
        let state = &pointer.button_states[0];
        assert!(state.is_depressed);
        match state.button.as_ref().and_then(|b| b.kind) {
            Some(proto::button::Kind::MappedButton(value)) => {
                assert_eq!(MappedButton::try_from(value).unwrap(), MappedButton::Left);
            }
            other => panic!("expected mapped button, got {other:?}"),
        }
        let loc = pointer.location.expect("location present");
        assert_eq!(loc.x_position as usize, 10);
        assert_eq!(loc.y_position as usize, 20);
    }

    #[test]
    fn notification_pointer_to_pointer_event_without_buttons() {
        let notif = UserEvent::Pointer {
            pos: Point::new(5, 6),
        };
        let message = c2s::Message::from(notif);
        let pointer = match message {
            c2s::Message::PointerEvent(pe) => pe,
            other => panic!("expected pointer event, got {other:?}"),
        };
        assert!(pointer.button_states.is_empty());
        let loc = pointer.location.expect("location present");
        assert_eq!(loc.x_position as usize, 5);
        assert_eq!(loc.y_position as usize, 6);
    }

    #[test]
    fn notification_key_to_key_event() {
        let notif = UserEvent::Key {
            code: 123,
            is_pressed: false,
        };
        let message = c2s::Message::from(notif);
        let key_event = match message {
            c2s::Message::KeyEvent(ke) => ke,
            other => panic!("expected key event, got {other:?}"),
        };
        assert_eq!(key_event.key, 123);
        assert!(!key_event.down);
    }

    #[test]
    fn notification_display_change_to_display_event() {
        let display = Display {
            area: Rect::new(Point::new(0, 0), Size::new(1920, 1080)),
            is_enabled: true,
        };
        let notif = UserEvent::DisplayChange {
            displays: vec![display.clone()],
        };
        let message = c2s::Message::from(notif);
        let display_event = match message {
            c2s::Message::DisplayEvent(de) => de,
            other => panic!("expected display event, got {other:?}"),
        };
        assert_eq!(display_event.displays.len(), 1);
        let info = &display_event.displays[0];
        assert!(info.is_enabled);
        let rectangle = info.area.as_ref().expect("rectangle present");
        let exported: Rectangle = display.area.clone().try_into().expect("rect to proto");
        assert_eq!(rectangle, &exported);
    }

    fn build_pixel_data(width: usize, height: usize) -> Vec<u8> {
        (0..(width * height))
            .flat_map(|idx| {
                let val = idx as u8;
                [val, val, val, 0xFF]
            })
            .collect()
    }

    fn build_cursor_image(width: u32, height: u32, data: Vec<u8>) -> proto::CursorImage {
        proto::CursorImage {
            id: 1,
            width,
            height,
            x_hot: 1,
            y_hot: 2,
            data,
        }
    }

    #[test]
    fn cursor_try_from_success() {
        let width = 2;
        let height = 2;
        let data = build_pixel_data(width, height);
        let cursor_image = build_cursor_image(width as u32, height as u32, data.clone());
        let cursor = Cursor::try_from(cursor_image).expect("cursor conversion succeeds");
        assert_eq!(cursor.hot, Point::new(1, 2));
        // ndarray stores row-major as (rows = height, cols = width)
        assert_eq!(cursor.pixels.shape(), &[height, width]);
        for ((x, y), pixel) in cursor.pixels.indexed_iter() {
            // Row-major linear index = row * width + col
            let idx = (x * width + y) as u8;
            let expected = Pixel32::from([idx, idx, idx, 0xFF]);
            assert_eq!(*pixel, expected);
        }
    }

    #[test]
    fn cursor_try_from_dimension_mismatch() {
        let width = 2;
        let height = 2;
        let mut data = build_pixel_data(width, height);
        data.pop(); // force mismatch
        let cursor_image = build_cursor_image(width as u32, height as u32, data);
        let err = Cursor::try_from(cursor_image).expect_err("dimension mismatch fails");
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn cursor_try_from_dimension_overflow() {
        let cursor_image = proto::CursorImage {
            id: 1,
            width: u32::MAX,
            height: 1,
            x_hot: 0,
            y_hot: 0,
            data: vec![],
        };
        let err = Cursor::try_from(cursor_image).expect_err("overflow should fail");
        assert!(matches!(err, Error::Protocol(_)));
    }
}
