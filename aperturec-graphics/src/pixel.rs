use crate::axis;
use crate::geometry::*;

use bytemuck::{Pod, Zeroable};
use ndarray::{ArcArray2, AssignElem, Data, DataMut, FoldWhile, Zip, prelude::*};
use std::mem;
use wide::{CmpEq, u32x8};

/// A 24-bit pixel in B-G-R order (no alpha channel).
#[derive(PartialEq, Debug, Default, Clone, Copy, Pod, Zeroable)]
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

/// A shared 2D array of Pixel24, alias for `ArcArray2<Pixel24>`.
pub type Pixel24MapShared = ArcArray2<Pixel24>;

/// A 32-bit pixel in B-G-R-A order, with explicit alpha.
/// A 32-bit pixel in B-G-R-A order.
///
/// The `#[repr(C, align(4))]` ensures 4-byte alignment so that
/// rows of `Pixel32` can be safely reinterpreted as `u32` chunks
/// for SIMD comparisons.
#[derive(PartialEq, Debug, Default, Clone, Copy, Pod, Zeroable)]
#[repr(C, align(4))]
pub struct Pixel32 {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

// Static‐assert that Pixel32 is exactly 4 bytes (no padding)
const _: () = assert!(std::mem::size_of::<Pixel32>() == 4);

impl Pixel32 {
    fn as_simdable_chunks<const SIMD_WIDTH: usize>(
        s: &[Self],
    ) -> (&[[u32; SIMD_WIDTH]], &[Pixel32]) {
        let (chunks, remainder) = s.as_chunks::<{ SIMD_WIDTH }>();
        // SAFETY: Pixel32 is #[repr(C, align(4))] with exactly four u8 fields (no padding),
        // so [Pixel32; SIMD_WIDTH] and [u32; SIMD_WIDTH] have identical size, alignment,
        // and layout. We cast the slice’s pointer and reconstruct a &[u32; SIMD_WIDTH]
        // slice with the same length, which is properly aligned.
        let ptr = chunks.as_ptr() as *const [u32; SIMD_WIDTH];
        let chunks = unsafe { std::slice::from_raw_parts(ptr, chunks.len()) };

        (chunks, remainder)
    }
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

pub trait Pixel32MapExt: PixelMap<Pixel = Pixel32> {
    fn simd_eq(&self, other: &impl PixelMap<Pixel = Pixel32>) -> bool {
        let a = self.as_ndarray();
        let b = other.as_ndarray();
        // Bail out early on shape mismatch so we don't ignore extra rows/cols.
        if a.dim() != b.dim() {
            return false;
        }
        Zip::from(a.rows())
            .and(b.rows())
            .fold_while(true, |acc, a, b| {
                if !acc {
                    FoldWhile::Done(false)
                } else if let (Some(a), Some(b)) =
                    (a.as_slice_memory_order(), b.as_slice_memory_order())
                {
                    // Fast-path supporting row-by-row SIMD comparison
                    let (a_chunks, a_rem) = Pixel32::as_simdable_chunks::<8>(a);
                    let (b_chunks, b_rem) = Pixel32::as_simdable_chunks::<8>(b);
                    for (&a_chunk, &b_chunk) in a_chunks.iter().zip(b_chunks) {
                        if !u32x8::from(a_chunk).simd_eq(u32x8::from(b_chunk)).all() {
                            return FoldWhile::Done(false);
                        }
                    }
                    FoldWhile::Continue(a_rem == b_rem)
                } else {
                    // Slow-path using element-wise comparison
                    FoldWhile::Continue(b == a)
                }
            })
            .into_inner()
    }
}

/// A 2D array of Pixel32, alias for `Array2<Pixel32>`.
pub type Pixel32Map = Array2<Pixel32>;

/// Extension trait for 2D pixel maps of `Pixel32` providing
/// a SIMD-accelerated equality comparison.
///
/// The `simd_eq` method returns `true` if every pixel in `self`
/// equals the corresponding pixel in `other`.  It first tries
/// a fast‐path by viewing each row as contiguous `u32x8` chunks
/// and comparing with SIMD; if that isn’t possible (e.g. non‐
/// contiguous memory), it falls back to a scalar element‐wise
/// comparison.
impl<T: PixelMap<Pixel = Pixel32>> Pixel32MapExt for T {}

/// A generic pixel type which can be compared to Pixel32, copied, sent and shared.
pub trait Pixel: PartialEq<Pixel32> + Copy + Send + Sync {}

impl<P> Pixel for P where P: PartialEq<Pixel32> + Copy + Send + Sync {}

/// A read-only 2D view into pixel data.
pub trait PixelMap: Sized {
    type Pixel: Pixel;

    fn as_ndarray(&self) -> ArrayView2<'_, Self::Pixel>;

    fn size(&self) -> Size {
        Size::new(
            self.as_ndarray().len_of(axis::X),
            self.as_ndarray().len_of(axis::Y),
        )
    }
}

/// A mutable 2D view into pixel data.
pub trait PixelMapMut: PixelMap {
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<'_, Self::Pixel>;
}

impl<E, P> PixelMap for ArrayBase<E, Ix2>
where
    P: Pixel,
    E: Data<Elem = P>,
{
    type Pixel = P;

    fn as_ndarray(&self) -> ArrayView2<'_, Self::Pixel> {
        self.view()
    }
}

impl<T: PixelMap> PixelMap for &T {
    type Pixel = T::Pixel;

    fn as_ndarray(&self) -> ArrayView2<'_, Self::Pixel> {
        <T as PixelMap>::as_ndarray(*self)
    }
}

impl<T: PixelMap> PixelMap for &mut T {
    type Pixel = T::Pixel;

    fn as_ndarray(&self) -> ArrayView2<'_, Self::Pixel> {
        <T as PixelMap>::as_ndarray(*self)
    }
}

impl<E, P> PixelMapMut for ArrayBase<E, Ix2>
where
    P: Pixel,
    E: DataMut<Elem = P>,
{
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<'_, Self::Pixel> {
        self.view_mut()
    }
}

impl<T: PixelMapMut> PixelMapMut for &mut T {
    fn as_ndarray_mut(&mut self) -> ArrayViewMut2<'_, Self::Pixel> {
        <T as PixelMapMut>::as_ndarray_mut(*self)
    }
}

// ------------------------------------------------------------------------------------------------
// tests for alpha-aware behavior
// ------------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;
    use std::mem::MaybeUninit;

    #[test]
    fn bytemuck_pixel24_cast_slice_round_trip() {
        let bytes = [1u8, 2, 3, 4, 5, 6];
        let pixels: &[Pixel24] = bytemuck::cast_slice(&bytes);
        let expected = [
            Pixel24 {
                blue: 1,
                green: 2,
                red: 3,
            },
            Pixel24 {
                blue: 4,
                green: 5,
                red: 6,
            },
        ];
        assert_eq!(pixels, &expected[..]);

        let bytes_round_trip: &[u8] = bytemuck::cast_slice(pixels);
        assert_eq!(bytes_round_trip, &bytes[..]);
    }

    #[test]
    fn bytemuck_pixel32_cast_slice_round_trip() {
        let pixels = [
            Pixel32 {
                blue: 1,
                green: 2,
                red: 3,
                alpha: 4,
            },
            Pixel32 {
                blue: 5,
                green: 6,
                red: 7,
                alpha: 8,
            },
        ];

        let bytes: &[u8] = bytemuck::cast_slice(&pixels);
        let expected_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(bytes, &expected_bytes[..]);

        let pixels_round_trip: &[Pixel32] = bytemuck::cast_slice(bytes);
        assert_eq!(pixels_round_trip, &pixels[..]);
    }

    #[test]
    fn bytemuck_zeroed_pixels_are_all_zero() {
        let p24 = <Pixel24 as bytemuck::Zeroable>::zeroed();
        assert_eq!(p24, Pixel24::default());

        let p32 = <Pixel32 as bytemuck::Zeroable>::zeroed();
        assert_eq!(p32, Pixel32::default());
    }

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

    #[test]
    fn simd_eq_true_large_equal() {
        // dimensions ≥ 8 columns to trigger at least one SIMD chunk
        let rows = 4;
        let cols = 16;
        let pixel = Pixel32 {
            blue: 1,
            green: 2,
            red: 3,
            alpha: 4,
        };
        let a: Array2<Pixel32> = Array2::from_elem((rows, cols), pixel);
        let b = a.clone();
        assert!(
            a.simd_eq(&b),
            "simd_eq should return true for identical maps"
        );
    }

    #[test]
    fn simd_eq_false_different_pixel() {
        // two maps that differ by exactly one pixel in the first SIMD chunk
        let rows = 2;
        let cols = 16;
        let a: Array2<Pixel32> = Array2::from_elem((rows, cols), Pixel32::default());
        let mut b = a.clone();
        // flip one pixel in row 0, col 5
        b[(0, 5)] = Pixel32 {
            blue: 10,
            green: 11,
            red: 12,
            alpha: 13,
        };
        assert!(
            !a.simd_eq(&b),
            "simd_eq should detect a single-pixel difference"
        );
    }

    #[test]
    fn simd_eq_false_mismatched_height() {
        let row = Pixel32::default();
        let a = Array2::from_elem((1, 4), row);
        let b = Array2::from_elem((2, 4), row);
        assert!(
            !a.simd_eq(&b) && !b.simd_eq(&a),
            "Different heights must not compare equal"
        );
    }

    #[test]
    fn simd_eq_false_width_multiple_of_chunk() {
        // Widths that differ by a SIMD chunk should not compare equal.
        let a = Array2::from_elem((2, 16), Pixel32::default());
        let b = Array2::from_elem((2, 24), Pixel32::default());
        assert!(
            !a.simd_eq(&b) && !b.simd_eq(&a),
            "Extra columns must be detected even when rows share initial chunks"
        );
    }
}
