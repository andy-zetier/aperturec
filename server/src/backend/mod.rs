use crate::task::encoder::{Point, RawPixel, Rect, Size};

use anyhow::Result;
use aperturec_protocol::common::*;
use aperturec_protocol::event as em;
use aperturec_protocol::event::client_to_server as em_c2s;
use futures::stream::Stream;
use ndarray::{s, Array2, ArrayView2};
use std::error::Error;
use std::fmt;
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

#[derive(Debug)]
pub struct FramebufferUpdate {
    pub rect: Rect,
    pub data: Array2<RawPixel>,
}

impl FramebufferUpdate {
    pub fn get_intersecting_data<'s>(
        &'s self,
        other: &Rect,
    ) -> Option<(Point, ArrayView2<'s, RawPixel>)> {
        if let Some(intersecting) = self.rect.intersection(other) {
            let int_box = intersecting.to_box2d();
            let slice = self.data.slice(s![
                (int_box.min.x - self.rect.origin.x)..(int_box.max.x - self.rect.origin.x),
                (int_box.min.y - self.rect.origin.y)..(int_box.max.y - self.rect.origin.y),
            ]);
            Some((intersecting.origin, slice))
        } else {
            None
        }
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
        size: Dimension,
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
                size: display_event.display_size.ok_or(EventError)?,
            },
        })
    }
}

mod backend_trait {
    use super::*;

    #[trait_variant::make(Backend: Send + Sync)]
    pub trait LocalBackend: Sized + Send + Sync + fmt::Debug {
        async fn initialize<N>(
            max_width: N,
            max_height: N,
            root_process_cmd: Option<&mut Command>,
        ) -> Result<Self>
        where
            N: Into<Option<usize>> + Send + Sync;
        async fn notify_event(&mut self, event: Event) -> Result<()>;
        async fn cursor_bitmaps(&self) -> Result<Vec<CursorBitmap>>;
        async fn set_resolution(&mut self, resolution: &Size) -> Result<()>;
        async fn resolution(&self) -> Result<Size>;
        async fn damage_stream(&self) -> Result<impl Stream<Item = Rect> + Send + Unpin + 'static>;
        async fn capture_area(&self, area: Rect) -> Result<FramebufferUpdate>;
        async fn exec_command(&mut self, command: &mut Command) -> Result<Child>;
        async fn wait_root_process(&mut self) -> Result<ExitStatus>;
        fn start_kill_root_process(&mut self) -> Result<()>;
        fn root_process_exited(&self) -> bool;
    }
}
pub use backend_trait::Backend;
