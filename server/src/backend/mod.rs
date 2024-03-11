use crate::task::encoder::{Point, RawPixel, Rect, Size};

use anyhow::Result;
use aperturec_protocol::common::*;
use aperturec_protocol::event as em;
use aperturec_protocol::event::client_to_server as em_c2s;
use async_trait::async_trait;
use futures::stream::Stream;
use ndarray::{s, Array2, ArrayView2};
use std::error::Error;
use std::fmt;
use std::pin::Pin;

pub mod x;
pub use x::X;

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

pub type DamageStream = Pin<Box<dyn Stream<Item = Rect> + Send>>;

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

#[async_trait]
pub trait Backend: Sized + Send + Sync + fmt::Debug {
    async fn initialize<N>(max_width: N, max_height: N) -> Result<Self>
    where
        N: Into<Option<usize>> + Send;
    async fn notify_event(&mut self, event: Event) -> Result<()>;
    async fn cursor_bitmaps(&self) -> Result<Vec<CursorBitmap>>;
    async fn set_resolution(&mut self, resolution: &Size) -> Result<()>;
    async fn resolution(&self) -> Result<Size>;
    async fn damage_stream(&self) -> Result<DamageStream>;
    async fn capture_area(&self, area: Rect) -> Result<FramebufferUpdate>;
    async fn start_root_process<S>(&mut self, root_process: S, should_replace: bool) -> Result<()>
    where
        S: AsRef<str> + Send;
    async fn wait_root_process(&mut self) -> Result<()>;
    fn root_process_exited(&self) -> bool;
}
