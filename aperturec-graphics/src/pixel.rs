use crate::axis;
use crate::geometry::*;

use ndarray::{AssignElem, Data, DataMut, prelude::*};
use std::mem;

/// A 24-bit pixel in B-G-R order (no alpha channel).
#[derive(PartialEq, Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct Pixel24 {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
}

impl PartialEq<Pixel32> for Pixel24 {
    fn eq(&self, rhs: &Pixel32) -> bool {
        self.blue == rhs.blue
            && self.green == rhs.green
            && self.red == rhs.red
            && rhs.alpha == u8::MAX
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

impl AssignElem<Pixel32> for &mut Pixel24 {
    fn assign_elem(self, input: Pixel32) {
        self.blue = input.blue;
        self.green = input.green;
        self.red = input.red;
    }
}

impl AssignElem<Pixel32> for &mut mem::MaybeUninit<Pixel24> {
    fn assign_elem(self, input: Pixel32) {
        // SAFETY: `self` is uninitialized memory for a repr(C) Pixel24 (3 u8s).
        // assume_init_mut returns a &mut Pixel24 which we immediately
        // initialize by assigning all fields from the Pixel32 input (copying
        // exactly 3 bytes), leaving no padding uninitialized.
        unsafe { self.assume_init_mut().assign_elem(input) };
    }
}

impl AssignElem<Pixel24> for &mut Pixel32 {
    fn assign_elem(self, input: Pixel24) {
        self.blue = input.blue;
        self.green = input.green;
        self.red = input.red;
        self.alpha = u8::MAX;
    }
}

/// A 2D array of Pixel24, alias for `Array2<Pixel24>`.
pub type Pixel24Map = Array2<Pixel24>;

/// A 32-bit pixel in B-G-R-A order, with explicit alpha.
#[derive(PartialEq, Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct Pixel32 {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

impl From<[u8; 4]> for Pixel32 {
    fn from(data: [u8; 4]) -> Self {
        Pixel32 {
            blue: data[0],
            green: data[1],
            red: data[2],
            alpha: data[3],
        }
    }
}

/// A 2D array of Pixel32, alias for `Array2<Pixel32>`.
pub type Pixel32Map = Array2<Pixel32>;

/// A generic pixel type which can be compared to Pixel32, copied, sent and shared.
pub trait Pixel: PartialEq<Pixel32> + Copy + Send + Sync {}

impl<P> Pixel for P where P: PartialEq<Pixel32> + Copy + Send + Sync {}

/// A read-only 2D view into pixel data.
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

/// A mutable 2D view into pixel data.
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

// ------------------------------------------------------------------------------------------------
// tests for alpha-aware behavior
// ------------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;

    #[test]
    fn pixel24_eqs_pixel32_when_alpha_is_max() {
        let p24 = Pixel24 {
            blue: 1,
            green: 2,
            red: 3,
        };
        let p32 = Pixel32 {
            blue: 1,
            green: 2,
            red: 3,
            alpha: u8::MAX,
        };
        assert_eq!(p24, p32);
    }

    #[test]
    fn pixel24_not_eq_pixel32_when_alpha_not_max() {
        let p24 = Pixel24 {
            blue: 5,
            green: 6,
            red: 7,
        };
        let p32 = Pixel32 {
            blue: 5,
            green: 6,
            red: 7,
            alpha: 254,
        };
        assert_ne!(p24, p32);
    }

    #[test]
    fn from_pixel24_to_pixel32_sets_alpha_max() {
        let p24 = Pixel24 {
            blue: 4,
            green: 5,
            red: 6,
        };
        let p32 = Pixel32::from(p24);
        assert_eq!(p32.blue, 4);
        assert_eq!(p32.green, 5);
        assert_eq!(p32.red, 6);
        assert_eq!(p32.alpha, u8::MAX);
    }

    #[test]
    fn assign_elem_to_maybeuninit_pixel24_initializes_all_fields() {
        let mut uninit: MaybeUninit<Pixel24> = MaybeUninit::uninit();
        let src = Pixel32 {
            blue: 7,
            green: 8,
            red: 9,
            alpha: u8::MAX,
        };
        // use the AssignElem<Pixel32> for MaybeUninit<Pixel24>
        unsafe {
            (&mut uninit).assign_elem(src);
            let p24 = uninit.assume_init();
            assert_eq!(p24.blue, 7);
            assert_eq!(p24.green, 8);
            assert_eq!(p24.red, 9);
        }
    }

    #[test]
    fn assign_elem_pixel24_into_pixel32_sets_alpha_max() {
        let mut dst = Pixel32::default();
        let src = Pixel24 {
            blue: 10,
            green: 11,
            red: 12,
        };
        (&mut dst).assign_elem(src);
        assert_eq!(
            dst,
            Pixel32 {
                blue: 10,
                green: 11,
                red: 12,
                alpha: u8::MAX
            }
        );
    }
}
