use aperturec_graphics::prelude::*;

use crate::frame::*;

use anyhow::{ensure, Result};
use gtk::cairo::{Format, ImageSurface};
use ndarray::{prelude::*, Zip};

#[derive(Debug, Clone)]
pub struct Image {
    pub display_config_id: u64,
    pub pixels: Pixel32Map,
    pub responsible_frames: Array2<usize>,
    pub update_history: BoxSet,
}

impl Image {
    pub fn new(size: Size, display_config_id: u64) -> Self {
        Image {
            display_config_id,
            pixels: Pixel32Map::from_elem(size.as_shape(), Pixel32::default()),
            responsible_frames: Array2::zeros(size.as_shape()),
            update_history: BoxSet::default(),
        }
    }

    pub fn area(&self) -> Box2D {
        Box2D::from_size(self.size())
    }

    pub fn size(&self) -> Size {
        self.pixels.size()
    }

    pub fn clear_history(&mut self) {
        self.update_history.clear()
    }

    pub fn copy_updates(&mut self, src: &Image) -> Result<()> {
        ensure!(
            src.display_config_id == self.display_config_id,
            format!(
                "src DCI {:?} != {:?}",
                src.display_config_id, self.display_config_id
            )
        );

        for area in &src.update_history {
            if let Some(isect) = self.area().intersection(area) {
                self.pixels
                    .as_ndarray_mut()
                    .slice_mut(isect.as_slice())
                    .assign(&src.pixels.as_ndarray().slice(isect.as_slice()));
                self.responsible_frames
                    .slice_mut(isect.as_slice())
                    .assign(&src.responsible_frames.slice(isect.as_slice()));
            }
        }

        // History is no longer valid now that we've copied updates from another Image
        self.clear_history();

        Ok(())
    }

    /// Calls the given closure with a temporary Cairo image surface. After the closure has returned
    /// there must be no further references to the surface.
    pub fn with_surface<R, F: FnOnce(&ImageSurface) -> R>(&mut self, func: F) -> R {
        let stride = Format::Rgb24
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

    pub fn draw(&mut self, draw: &Draw) {
        let isect = match self.area().intersection(&draw.area()) {
            Some(isect) => isect,
            None => return,
        };

        let src = draw.pixels.slice(Box2D::from_size(isect.size()).as_slice());
        let mut dst = self.pixels.slice_mut(isect.as_slice());
        let mut responsible = self.responsible_frames.slice_mut(isect.as_slice());

        let mut did_assignment = false;
        Zip::from(src)
            .and(&mut dst)
            .and(&mut responsible)
            .for_each(|src, dst, responsible| {
                if *responsible <= draw.frame {
                    *dst = Pixel32::from(*src);
                    *responsible = draw.frame;
                    did_assignment = true;
                }
            });

        if did_assignment {
            self.update_history.add(draw.area());

            #[cfg(feature = "draw-debug")]
            {
                let b = Box2D::new(
                    draw.origin,
                    draw.origin
                        + Point::new(draw.pixels.len_of(axis::X), draw.pixels.len_of(axis::Y))
                            .to_vector(),
                );
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
    use super::*;
    use test_log::test;

    #[test]
    fn copy_updates_simple() {
        let size = Size::new(1920, 1080);
        let mut image1 = Image::new(size, 0);
        let mut image2 = image1.clone();

        let draw = Draw {
            frame: 1,
            origin: Point::new(0, 0),
            pixels: Array2::from_elem(
                size.as_shape(),
                Pixel24 {
                    blue: 100,
                    green: 200,
                    red: 50,
                },
            ),
        };

        image2.draw(&draw);
        image1
            .copy_updates(&image2)
            .expect("Failed to copy updates");
        assert_eq!(image1.pixels, image2.pixels);
        assert_eq!(image1.responsible_frames, image2.responsible_frames);
    }

    #[test]
    fn copy_updates_out_of_order_frames() {
        let size = Size::new(1920, 1080);
        let mut image1 = Image::new(size, 1);
        let mut image2 = image1.clone();

        let mut draw = Draw {
            frame: 2,
            origin: Point::new(0, 0),
            pixels: Array2::from_elem(
                (540, 1080),
                Pixel24 {
                    blue: 0,
                    green: 0,
                    red: 255,
                },
            ),
        };
        image2.draw(&draw);
        image1
            .copy_updates(&image2)
            .expect("Failed to copy updates");
        assert_eq!(image1.pixels, image2.pixels);
        assert_eq!(image1.responsible_frames, image2.responsible_frames);

        draw.frame = 3;
        draw.origin = Point::new(0, 0);
        draw.pixels = Array2::from_elem(
            (270, 480),
            Pixel24 {
                blue: 0,
                green: 255,
                red: 0,
            },
        );
        image1.draw(&draw);
        image2
            .copy_updates(&image1)
            .expect("Failed to copy updates");
        assert_eq!(image1.pixels, image2.pixels);
        assert_eq!(image1.responsible_frames, image2.responsible_frames);

        draw.frame = 1;
        draw.origin = Point::new(0, 0);
        draw.pixels = Array2::from_elem(
            (135, 240),
            Pixel24 {
                blue: 255,
                green: 0,
                red: 0,
            },
        );
        image2.draw(&draw);
        image1
            .copy_updates(&image2)
            .expect("Failed to copy updates");
        assert_eq!(image1.pixels, image2.pixels);
        assert_eq!(image1.responsible_frames, image2.responsible_frames);
    }

    #[test]
    fn copy_updates_partial_overlapping() {
        let size = Size::new(1920, 1080);
        let mut image1 = Image::new(size, 2);
        let mut image2 = image1.clone();

        let mut draw = Draw {
            frame: 1,
            origin: Point::new(0, 0),
            pixels: Array2::from_elem(
                (540, 1920),
                Pixel24 {
                    blue: 255,
                    green: 255,
                    red: 255,
                },
            ),
        };

        image1.draw(&draw);
        draw.frame = 2;
        draw.origin = Point::new(480, 0);
        draw.pixels = Array2::from_elem(
            (1080, 960),
            Pixel24 {
                blue: 255,
                green: 0,
                red: 0,
            },
        );

        image1.draw(&draw);
        image2
            .copy_updates(&image1)
            .expect("Failed to copy updates");
        assert_eq!(image1.pixels, image2.pixels);
        assert_eq!(image1.responsible_frames, image2.responsible_frames);
    }
}
