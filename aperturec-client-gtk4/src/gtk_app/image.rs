//! In-memory pixel buffer with Cairo surface interop.

use aperturec_graphics::prelude::*;

use gtk4::cairo::{Format, ImageSurface};
use ndarray::{Array2, ArrayView2, Zip};
use tracing::*;

/// RGBA image buffer with frame-aware draw ordering.
#[derive(Debug, Clone)]
pub struct Image {
    /// RGBA pixels for the current frame buffer.
    pixels: Pixel32Map,
    /// Frame index responsible for each pixel (used for ordering).
    responsible_frames: Array2<usize>,
}

impl Image {
    /// Return the image dimensions.
    pub fn size(&self) -> Size {
        self.pixels.size()
    }

    /// Create a transparent image with the given size.
    pub fn new(size: Size) -> Self {
        Self::with_rgba(size, 0, 0, 0, 0)
    }

    /// Create an opaque black image.
    pub fn black(size: Size) -> Self {
        Self::with_rgb(size, 0, 0, 0)
    }

    /// Create an opaque green image (debug helper).
    #[allow(unused)]
    pub fn green(size: Size) -> Self {
        Self::with_rgb(size, 0, u8::MAX, 0)
    }

    /// Create an opaque blue image (debug helper).
    #[allow(unused)]
    pub fn blue(size: Size) -> Self {
        Self::with_rgb(size, 0, 0, u8::MAX)
    }

    /// Create an opaque red image (debug helper).
    #[allow(unused)]
    pub fn red(size: Size) -> Self {
        Self::with_rgb(size, u8::MAX, 0, 0)
    }

    /// Create an opaque image with the supplied RGB color.
    pub fn with_rgb(size: Size, red: u8, green: u8, blue: u8) -> Self {
        Self::with_rgba(size, red, green, blue, u8::MAX)
    }

    /// Create an image with the supplied RGBA color.
    pub fn with_rgba(size: Size, red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        let pixel = Pixel32 {
            red,
            green,
            blue,
            alpha,
        };
        Image {
            pixels: Pixel32Map::from_elem(size.as_shape(), pixel),
            responsible_frames: Array2::zeros(size.as_shape()),
        }
    }

    /// Call a closure with a temporary Cairo surface backed by this image.
    ///
    /// The surface must not be retained beyond the closure.
    pub fn with_surface<R, F: FnOnce(&ImageSurface) -> R>(&mut self, func: F) -> R {
        let stride = Format::ARgb32
            .stride_for_width(self.pixels.size().width as u32)
            .expect("create stride");

        let data = self.pixels.as_ndarray_mut().as_mut_ptr() as *mut u8;

        // SAFETY: The image surface will only exist during this function. This function takes &mut
        // self as a receiver, guaranteeing that we have exclusive access to self.pixels, so the
        // raw pointer to self.pixels will not be changed out from under us
        let surface = unsafe {
            ImageSurface::create_for_data_unsafe(
                data,
                Format::ARgb32,
                self.pixels.size().width as i32,
                self.pixels.size().height as i32,
                stride,
            )
        }
        .expect("create surface");

        func(&surface)
    }

    /// Draw an RGB pixel patch at the provided origin, honoring frame ordering.
    pub fn draw(&mut self, src: ArrayView2<Pixel24>, dst_origin: Point, frame: usize) {
        trace!(?dst_origin, frame, "image draw");
        let dst_area = Rect::new(dst_origin, src.size());
        let image_bounds = Rect::from_size(self.size());
        let Some(dst_area) = dst_area.intersection(&image_bounds) else {
            trace!(?dst_origin, frame, "draw outside image bounds; dropping");
            return;
        };

        if dst_area != Rect::new(dst_origin, src.size()) {
            trace!(?dst_area, frame, "clipping draw to image bounds");
        }

        let src_offset = Point::new(
            dst_area.origin.x - dst_origin.x,
            dst_area.origin.y - dst_origin.y,
        );
        let src_area = Rect::new(src_offset, dst_area.size);
        let src = src.slice(src_area.as_slice());

        let mut dst_pixels = self.pixels.slice_mut(dst_area.as_slice());
        let mut dst_responsible = self.responsible_frames.slice_mut(dst_area.as_slice());

        let mut did_assignment = false;
        Zip::from(src)
            .and(&mut dst_pixels)
            .and(&mut dst_responsible)
            .for_each(|src, dst, responsible| {
                if *responsible <= frame {
                    *dst = Pixel32::from(*src);
                    *responsible = frame;
                    did_assignment = true;
                }
            });

        #[cfg(feature = "draw-debug")]
        {
            if did_assignment {
                let b = dst_area.to_box2d();
                for x in b.min.x..b.max.x {
                    self.pixels[(b.min.y, x)] = Pixel32 {
                        blue: 0,
                        green: u8::MAX,
                        red: 0,
                        alpha: u8::MAX,
                    };
                    self.pixels[(b.max.y - 1, x)] = Pixel32 {
                        blue: 0,
                        green: u8::MAX,
                        red: 0,
                        alpha: u8::MAX,
                    };
                }
                for y in b.min.y..b.max.y {
                    self.pixels[(y, b.min.x)] = Pixel32 {
                        blue: 0,
                        green: u8::MAX,
                        red: 0,
                        alpha: u8::MAX,
                    };
                    self.pixels[(y, b.max.x - 1)] = Pixel32 {
                        blue: 0,
                        green: u8::MAX,
                        red: 0,
                        alpha: u8::MAX,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Image, Pixel24, Pixel24Map, Pixel32, Point, Size};
    use aperturec_graphics::ndarray_convert::AsNdarrayShape;

    #[test]
    fn draw_respects_frame_ordering() {
        let size = Size::new(2, 2);
        let mut image = Image::black(size);

        let red = Pixel24 {
            blue: 0,
            green: 0,
            red: 200,
        };
        let blue = Pixel24 {
            blue: 200,
            green: 0,
            red: 0,
        };

        let src_red = Pixel24Map::from_elem(size.as_shape(), red);
        let src_blue = Pixel24Map::from_elem(size.as_shape(), blue);

        image.draw(src_red.view(), Point::zero(), 1);
        image.draw(src_blue.view(), Point::zero(), 0);

        assert_eq!(
            image.pixels[(0, 0)],
            Pixel32 {
                blue: 0,
                green: 0,
                red: 200,
                alpha: u8::MAX,
            }
        );

        image.draw(src_blue.view(), Point::zero(), 2);
        assert_eq!(
            image.pixels[(1, 1)],
            Pixel32 {
                blue: 200,
                green: 0,
                red: 0,
                alpha: u8::MAX,
            }
        );
    }

    #[test]
    fn with_surface_exposes_expected_dimensions() {
        let size = Size::new(3, 4);
        let mut image = Image::new(size);

        image.with_surface(|surface| {
            assert_eq!(surface.width(), size.width as i32);
            assert_eq!(surface.height(), size.height as i32);
        });
    }

    #[test]
    fn draw_clips_out_of_bounds() {
        let size = Size::new(2, 2);
        let mut image = Image::black(size);

        let red = Pixel24 {
            blue: 0,
            green: 0,
            red: 200,
        };
        let src = Pixel24Map::from_elem(Size::new(2, 2).as_shape(), red);

        image.draw(src.view(), Point::new(1, 1), 1);

        let black = Pixel32 {
            blue: 0,
            green: 0,
            red: 0,
            alpha: u8::MAX,
        };
        let expected_red = Pixel32::from(red);
        assert_eq!(image.pixels[(1, 1)], expected_red);
        assert_eq!(image.pixels[(0, 0)], black);
        assert_eq!(image.pixels[(0, 1)], black);
        assert_eq!(image.pixels[(1, 0)], black);
    }

    #[test]
    fn draw_out_of_bounds_is_noop() {
        let size = Size::new(2, 2);
        let mut image = Image::black(size);

        let red = Pixel24 {
            blue: 0,
            green: 0,
            red: 200,
        };
        let src = Pixel24Map::from_elem(Size::new(2, 2).as_shape(), red);

        image.draw(src.view(), Point::new(3, 3), 1);

        let black = Pixel32 {
            blue: 0,
            green: 0,
            red: 0,
            alpha: u8::MAX,
        };
        assert!(image.pixels.iter().all(|px| *px == black));
    }
}
