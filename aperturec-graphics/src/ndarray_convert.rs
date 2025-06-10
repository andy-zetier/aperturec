pub use ndarray::{SliceArg, prelude::*};

pub trait AsNdarrayShape {
    fn as_shape(&self) -> impl ShapeBuilder<Dim = Ix2>;
}

pub trait AsNdarraySlice {
    fn as_slice(&self) -> impl SliceArg<Ix2, OutDim = Dim<[usize; 2]>>;
}
