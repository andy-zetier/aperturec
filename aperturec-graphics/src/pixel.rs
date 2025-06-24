use crate::axis;
use crate::geometry::*;

use ndarray::{AssignElem, Data, DataMut, prelude::*};

#[derive(PartialEq, Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct Pixel24 {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
}

impl AsRef<[u8; 3]> for Pixel24 {
    fn as_ref(&self) -> &[u8; 3] {
        // SAFETY: The Pixel24 structure is repr(C) and 3 bytes
        unsafe { &*(self as *const Pixel24 as *const [u8; 3]) }
    }
}

impl AsMut<[u8; 3]> for Pixel24 {
    fn as_mut(&mut self) -> &mut [u8; 3] {
        // SAFETY: The Pixel24 structure is repr(C) and 3 bytes
        unsafe { &mut *(self as *mut Pixel24 as *mut [u8; 3]) }
    }
}

pub type Pixel24Map = Array2<Pixel24>;

#[derive(PartialEq, Debug, Default, Clone, Copy)]
pub struct Pixel32 {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

impl PartialEq<Pixel24> for Pixel32 {
    fn eq(&self, rhs: &Pixel24) -> bool {
        self.alpha == u8::MAX
            && self.blue == rhs.blue
            && self.green == rhs.green
            && self.red == rhs.red
    }
}

pub type Pixel32Map = Array2<Pixel32>;

impl AssignElem<Pixel24> for &mut Pixel32 {
    fn assign_elem(self, other: Pixel24) {
        self.blue = other.blue;
        self.green = other.green;
        self.red = other.red;
        self.alpha = u8::MAX;
    }
}

impl From<Pixel24> for Pixel32 {
    fn from(p24: Pixel24) -> Self {
        Pixel32 {
            blue: p24.blue,
            green: p24.green,
            red: p24.red,
            alpha: u8::MAX,
        }
    }
}

pub trait Pixel: PartialEq<Pixel24> + Copy + Send + Sync {}

impl<P> Pixel for P where P: PartialEq<Pixel24> + Copy + Send + Sync {}

pub trait PixelMap: Sized {
    type Pixel: Pixel;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel>;

    fn size(&self) -> Size {
        Size::new(
            self.as_ndarray().len_of(axis::X),
            self.as_ndarray().len_of(axis::Y),
        )
    }
}

pub trait PixelMapMut: PixelMap {
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<Self::Pixel>;
}

impl<E, P> PixelMap for ArrayBase<E, Ix2>
where
    P: Pixel,
    E: Data<Elem = P>,
{
    type Pixel = P;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel> {
        self.view()
    }
}

impl<T: PixelMap> PixelMap for &T {
    type Pixel = T::Pixel;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel> {
        <T as PixelMap>::as_ndarray(*self)
    }
}

impl<T: PixelMap> PixelMap for &mut T {
    type Pixel = T::Pixel;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel> {
        <T as PixelMap>::as_ndarray(*self)
    }
}

impl<E, P> PixelMapMut for ArrayBase<E, Ix2>
where
    P: Pixel,
    E: DataMut<Elem = P>,
{
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<Self::Pixel> {
        self.view_mut()
    }
}

impl<T: PixelMapMut> PixelMapMut for &mut T {
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<Self::Pixel> {
        <T as PixelMapMut>::as_ndarray_mut(*self)
    }
}
