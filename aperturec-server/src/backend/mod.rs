use aperturec_graphics::{display::Display, prelude::*};

use anyhow::Result;
use aperturec_protocol::common::*;
use aperturec_protocol::event as em;
use aperturec_protocol::event::client_to_server as em_c2s;
use futures::stream::Stream;
use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::process::ExitStatus;
use tokio::process::{Child, Command};

pub mod x;
pub use x::X;

#[derive(Debug)]
pub struct SwapableBackend<B: Backend> {
    root: B,
    client_specified: Option<B>,
}

impl<B: Backend> Deref for SwapableBackend<B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        match &self.client_specified {
            Some(client_specified_backend) => client_specified_backend,
            None => &self.root,
        }
    }
}

impl<B: Backend> DerefMut for SwapableBackend<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.client_specified {
            Some(client_specified_backend) => client_specified_backend,
            None => &mut self.root,
        }
    }
}

impl<B: Backend> SwapableBackend<B> {
    pub fn new(root: B) -> Self {
        SwapableBackend {
            root,
            client_specified: None,
        }
    }

    pub fn set_client_specified(&mut self, client_specified: B) -> Option<B> {
        self.client_specified.replace(client_specified)
    }

    pub fn into_inner(self) -> (B, Option<B>) {
        (self.root, self.client_specified)
    }

    pub fn into_root(self) -> B {
        self.into_inner().0
    }
}

impl<B: Backend> From<B> for SwapableBackend<B> {
    fn from(b: B) -> Self {
        Self::new(b)
    }
}

#[derive(Debug)]
pub enum Event {
    Key {
        key: u32,
        is_depressed: bool,
    },
    Pointer {
        location: Location,
        button_states: Vec<(em::Button, bool)>,
        cursor: Cursor,
    },
    Display {
        displays: Vec<Display>,
    },
}

#[derive(Debug)]
pub struct EventError;
impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <Self as fmt::Debug>::fmt(self, f)
    }
}

impl Error for EventError {}

impl TryFrom<em_c2s::Message> for Event {
    type Error = EventError;

    fn try_from(m: em_c2s::Message) -> Result<Event, EventError> {
        Ok(match m {
            em_c2s::Message::KeyEvent(key_event) => Event::Key {
                key: key_event.key,
                is_depressed: key_event.down,
            },
            em_c2s::Message::PointerEvent(pointer_event) => Event::Pointer {
                cursor: Cursor::try_from(pointer_event.cursor).map_err(|_| EventError)?,
                location: pointer_event.location.ok_or(EventError)?,
                button_states: pointer_event
                    .button_states
                    .into_iter()
                    .map(|bs| {
                        if bs.button.is_some() {
                            Ok((bs.button.unwrap(), bs.is_depressed))
                        } else {
                            Err(EventError)
                        }
                    })
                    .collect::<Result<_, EventError>>()?,
            },
            em_c2s::Message::DisplayEvent(display_event) => Event::Display {
                displays: display_event
                    .as_ref()
                    .iter()
                    .map(|di| Display::try_from(*di))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| EventError)?,
            },
        })
    }
}

#[derive(Debug)]
pub struct CursorChange {
    pub serial: u32,
}

#[derive(Debug)]
pub struct CursorImage {
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub x_hot: u16,
    pub y_hot: u16,
    pub data: Vec<u8>,
}

impl Hash for CursorImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Exclude serial
        self.width.hash(state);
        self.height.hash(state);
        self.x_hot.hash(state);
        self.y_hot.hash(state);
        self.data.hash(state);
    }
}

#[derive(Debug)]
pub struct LockState {
    pub is_caps_locked: Option<bool>,
    pub is_scroll_locked: Option<bool>,
    pub is_num_locked: Option<bool>,
}

pub struct SetDisplaysSuccess {
    pub changed: bool,
    pub displays: Vec<Display>,
}

mod backend_trait {
    use super::*;

    #[trait_variant::make(Backend: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalBackend: Sized + Send + Sync + fmt::Debug {
        type PixelMap: PixelMap + Send + Sync + 'static;

        async fn initialize<N>(
            max_width: N,
            max_height: N,
            max_display_count: N,
            root_process_cmd: Option<&mut Command>,
        ) -> Result<Self>
        where
            N: Into<Option<usize>> + Send + Sync;
        async fn notify_event(&mut self, event: Event) -> Result<()>;
        async fn set_lock_state(&self, lock_state: LockState) -> Result<()>;
        async fn clear_focus(&self) -> Result<()>;
        async fn set_displays(
            &mut self,
            requested_displays: Vec<Display>,
        ) -> Result<SetDisplaysSuccess, Vec<Display>>;
        async fn damage_stream(&self) -> Result<impl Stream<Item = Rect> + Send + Unpin + 'static>;
        async fn capture_area(&self, area: Rect) -> Result<Self::PixelMap>;
        async fn cursor_stream(
            &self,
        ) -> Result<impl Stream<Item = CursorChange> + Send + Unpin + 'static>;
        async fn capture_cursor(&self) -> Result<CursorImage>;
        async fn exec_command(&mut self, command: &mut Command) -> Result<Child>;
        async fn wait_root_process(&mut self) -> Result<ExitStatus>;
    }
}
pub use backend_trait::Backend;
