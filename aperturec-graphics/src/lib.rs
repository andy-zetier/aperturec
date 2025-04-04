pub mod axis;
pub mod boxset;
pub mod display;
pub mod euclid_collections;
pub mod geometry;
pub mod ndarray_convert;
pub mod partition;
pub mod pixel;
pub mod rectangle_cover;

pub mod prelude {
    pub use super::axis;
    pub use super::boxset::Set as BoxSet;
    pub use super::geometry::{Box2D, Point, Rect, Size, Vector};
    pub use super::ndarray_convert::{AsNdarrayShape as _, AsNdarraySlice as _};
    pub use super::pixel::{
        Pixel, Pixel24, Pixel24Map, Pixel32, Pixel32Map, PixelMap, PixelMapMut,
    };
}
