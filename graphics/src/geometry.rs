use crate::ndarray_convert::*;

use euclid::{Point2D, Size2D, UnknownUnit, Vector2D};

pub type Box2D = euclid::Box2D<usize, UnknownUnit>;
pub type Point = Point2D<usize, UnknownUnit>;
pub type Rect = euclid::Rect<usize, UnknownUnit>;
pub type Size = Size2D<usize, UnknownUnit>;
pub type Vector = Vector2D<i64, UnknownUnit>;

impl AsNdarrayShape for Box2D {
    fn as_shape(&self) -> impl ShapeBuilder<Dim = Ix2> {
        (self.height(), self.width())
    }
}

impl AsNdarrayShape for Rect {
    fn as_shape(&self) -> impl ShapeBuilder<Dim = Ix2> {
        (self.size.height, self.size.width)
    }
}

impl AsNdarrayShape for Size {
    fn as_shape(&self) -> impl ShapeBuilder<Dim = Ix2> {
        (self.height, self.width)
    }
}

impl AsNdarraySlice for Rect {
    fn as_slice(&self) -> impl SliceArg<Ix2, OutDim = Dim<[usize; 2]>> {
        s![
            self.origin.y..self.origin.y + self.size.height,
            self.origin.x..self.origin.x + self.size.width,
        ]
    }
}

impl AsNdarraySlice for Box2D {
    fn as_slice(&self) -> impl SliceArg<Ix2, OutDim = Dim<[usize; 2]>> {
        s![self.min.y..self.max.y, self.min.x..self.max.x]
    }
}
