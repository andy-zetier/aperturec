use crate::task::encoder::{Point, RawPixel, Rect, Size};

use anyhow::Result;
use aperturec_protocol::common_types::*;
use aperturec_protocol::event_messages as em;
use async_trait::async_trait;
use futures::stream::Stream;
use ndarray::{s, Array2, ArrayView2};
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
pub type Event = em::ClientToServerMessage;

#[async_trait]
pub trait Backend: Sized + Send + Sync + fmt::Debug {
    async fn initialize<N, S>(initial_program: S, max_width: N, max_height: N) -> Result<Self>
    where
        N: Into<Option<usize>> + Send,
        S: AsRef<str> + Send;
    async fn notify_event(&mut self, event: Event) -> Result<()>;
    async fn cursor_bitmaps(&self) -> Result<Option<Vec<CursorBitmap>>>;
    async fn set_resolution(&mut self, resolution: &Size) -> Result<()>;
    async fn resolution(&self) -> Result<Size>;
    async fn damage_stream(&self) -> Result<DamageStream>;
    async fn capture_area(&self, area: Rect) -> Result<FramebufferUpdate>;
}
