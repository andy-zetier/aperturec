use anyhow::{bail, Result};
use flate2::bufread::ZlibDecoder;
use flate2::Decompress;
use gtk::cairo::{Format, ImageSurface};
use std::io::Read;

//
// Custom Image type. Stores a heap allocated byte array for the pixels for each Decoder. Can be
// sent safely between UI and Client threads and can be temporarily converted to a Cairo image
// surface for drawing operations. Includes embedded origin info for the UI thread.
//
// Based heavily on https://github.com/gtk-rs/examples/blob/master/src/bin/cairo_threads.rs
//
#[derive(Debug, Clone)]
pub struct Image {
    width: u32,
    height: u32,
    origin_x: u32,
    origin_y: u32,
    pub decoder_id: usize,
    data: Option<Vec<u8>>,
    //data: Option<Box<[u8]>>,
    update_history: Vec<ImageArea>,
}

//
// Stores metadata related to updated pixel data within the Image
//
#[derive(Debug, Copy, Clone)]
pub struct ImageArea {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
}

impl ImageArea {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x: x.try_into().unwrap(),
            y: y.try_into().unwrap(),
            width: width.try_into().unwrap(),
            height: height.try_into().unwrap(),
            bytes_per_pixel: 3,
        }
    }
}

impl Image {
    // Blue + Green + Red + Alpha
    pub const BYTES_PER_PIXEL: usize = 4;

    pub fn new(width: u32, height: u32) -> Self {
        //
        // While unsigned types more accurately represent this type's fields, most of the
        // underlying GTK objects use i32. Ensure each of these can be safely converted into a
        // signed equivalent.
        //
        let _: i32 = width.try_into().expect("Failed to cast image width to i32");
        let _: i32 = height
            .try_into()
            .expect("Failed to cast image height to i32");

        Self {
            width,
            height,
            origin_x: 0,
            origin_y: 0,
            decoder_id: 0,
            data: Some(vec![
                0;
                Self::BYTES_PER_PIXEL * width as usize * height as usize
            ]),
            update_history: Vec::new(),
        }
    }

    pub fn origin(&self) -> (u32, u32) {
        (self.origin_x, self.origin_y)
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn origin_signed(&self) -> (i32, i32) {
        (self.origin_x as i32, self.origin_y as i32)
    }

    pub fn dims_signed(&self) -> (i32, i32) {
        (self.width as i32, self.height as i32)
    }

    pub fn clone_with_decoder_info(&self, decoder_id: usize, origin: (u32, u32)) -> Self {
        let mut new = self.clone();
        new.decoder_id = decoder_id;
        new.origin_x = origin.0;
        new.origin_y = origin.1;
        new
    }

    pub fn get_data(&self) -> &[u8] {
        self.data.as_ref().unwrap().as_ref()
    }

    pub fn clear_history(&mut self) {
        self.update_history.clear()
    }

    #[allow(clippy::identity_op)]
    pub fn copy_updates(&mut self, src_img: &Image) -> Result<()> {
        if src_img.decoder_id != self.decoder_id {
            bail!(
                "Cannot copy updates from image with decoder_id {}, expected {}.",
                src_img.decoder_id,
                self.decoder_id
            );
        }

        let dst = self.data.as_mut().unwrap();
        let src = src_img.data.as_ref().expect("src_img data is empty!");
        let width: usize = self.width.try_into().expect("Failed to convert width!");

        for ia in src_img.update_history.iter() {
            let pixel_count = ia.width * ia.height;
            let mut dst_row_idx: usize = ((ia.y * width) + ia.x) * Self::BYTES_PER_PIXEL;
            for pixel in 0..pixel_count {
                let dst_col_idx = pixel % ia.width;
                let idx = dst_row_idx + (dst_col_idx * Self::BYTES_PER_PIXEL);

                dst[idx + 0] = src[idx + 0];
                dst[idx + 1] = src[idx + 1];
                dst[idx + 2] = src[idx + 2];
                // Ignore Alpha

                if dst_col_idx == (ia.width - 1) {
                    // Move idx to the next row
                    dst_row_idx += width * Self::BYTES_PER_PIXEL;
                }
            }
        }

        // History is no longer valid now that we've copied updates from another Image
        self.clear_history();

        Ok(())
    }

    //
    // Calls the given closure with a temporary Cairo image surface. After the closure has returned
    // there must be no further references to the surface.
    //
    pub fn with_surface<F: Fn(&ImageSurface)>(&mut self, func: F) {
        // Temporary move out the pixels
        let image = self.data.take().expect("Empty image");

        let stride = 4 * self.width;
        // The surface will own the image for the scope of the block below
        let surface = ImageSurface::create_for_data(
            image,
            Format::Rgb24,
            self.width as i32,
            self.height as i32,
            stride as i32,
        )
        .expect("Can't create surface");
        func(&surface);

        self.data = Some(surface.take_data().expect("get data").to_vec());
    }

    pub fn draw_raw_zlib(
        &mut self,
        data: &[u8],
        x: u64,
        y: u64,
        width: u64,
        height: u64,
    ) -> Result<()> {
        let mut z = ZlibDecoder::new_with_decompress(data, Decompress::new(false));
        let mut decompressed = Vec::new();
        z.read_to_end(&mut decompressed)?;
        if (decompressed.len() as u64) < (width * height * 3) {
            log::warn!(
                "Decompressed < {}x{}x3 = {}",
                width,
                height,
                width * height * 3
            );
        }
        let max_idx = std::cmp::min(decompressed.len(), (width * height * 3) as usize);
        self.draw_raw(&decompressed[..max_idx], x, y, width, height)
    }

    pub fn draw_raw(&mut self, data: &[u8], x: u64, y: u64, width: u64, height: u64) -> Result<()> {
        self.draw_raw_area(
            data,
            ImageArea::new(
                x.try_into().expect("Failed to convert origin x"),
                y.try_into().expect("Failed to convert origin y"),
                width.try_into().expect("Failed to convert width"),
                height.try_into().expect("failed to convert height"),
            ),
        )
    }

    //
    // Draws "raw" data to the Image. Data must be a [u8] in BGR format such that the first byte is
    // Blue, the next is Green, followed by Red for each pixel. Pixel data is stored in a
    // row-oriented manner such that the first byte of the second row would be located at 3 *
    // width.
    //
    // Metadata related to the raw data is stored in an ImageArea. This struct defines where the
    // new data should be placed within the Image (x,y) and how large the raw data rectangle is
    // (width and height).
    //
    #[allow(clippy::identity_op)]
    pub fn draw_raw_area(&mut self, data: &[u8], ia: ImageArea) -> Result<()> {
        let width: usize = self.width.try_into().expect("Failed to convert width!");
        let height: usize = self.height.try_into().expect("Failed to convert height!");

        if (ia.x + ia.width) > width {
            bail!(
                "Error processing {:?}. Data is too wide: ({}+{}) > {}",
                &ia,
                ia.x,
                ia.width,
                width
            );
        }

        if (ia.y + ia.height) > height {
            bail!(
                "Error processing {:?}. Data is too tall: ({}+{}) > {}",
                &ia,
                ia.y,
                ia.height,
                height
            );
        }

        let pixel_count = ia.width * ia.height;

        if (pixel_count * ia.bytes_per_pixel) != data.len() {
            bail!(
                "Error processing {:?}. Data length is invalid: {}, expected {}*{}*{}={}",
                &ia,
                data.len(),
                ia.width,
                ia.height,
                ia.bytes_per_pixel,
                pixel_count * ia.bytes_per_pixel
            );
        }

        let dst = self.data.as_mut().unwrap();
        let mut dst_row_idx: usize = ((ia.y * width) + ia.x) * Self::BYTES_PER_PIXEL;

        for pixel in 0..pixel_count {
            let dst_col_idx = pixel % ia.width;
            let dst_idx = dst_row_idx + (dst_col_idx * Self::BYTES_PER_PIXEL);
            let src_idx = pixel * ia.bytes_per_pixel;

            dst[dst_idx + 0] = data[src_idx + 0]; // Blue
            dst[dst_idx + 1] = data[src_idx + 1]; // Green
            dst[dst_idx + 2] = data[src_idx + 2]; // Red

            if dst_col_idx == (ia.width - 1) {
                // Move destination index to the next row
                dst_row_idx += width * Self::BYTES_PER_PIXEL;
            }
        }

        self.update_history.push(ia);

        Ok(())
    }

    #[allow(clippy::identity_op)]
    pub fn print_data(&self) {
        let data = self.data.as_ref().expect("Image data is emtpy!");
        print!("     ");
        for c in 0..self.width {
            print!("{:>6}    ", c);
        }
        println!();
        for r in 0..self.height {
            print!("{:3} [ ", r);
            for c in 0..self.width {
                let pix_idx: usize = ((r * self.width * Self::BYTES_PER_PIXEL as u32)
                    + (c * Self::BYTES_PER_PIXEL as u32))
                    .try_into()
                    .unwrap();
                print!(
                    "({},{},{},{}) ",
                    data[pix_idx + 0],
                    data[pix_idx + 1],
                    data[pix_idx + 2],
                    data[pix_idx + 3]
                );
            }
            println!("]");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_new() {
        let image = Image::new(10, 10);
        let expect = vec![0; image.get_data().len()];
        assert_eq!(image.get_data(), expect);
    }

    #[test]
    fn copy_updates() {
        let mut image = Image::new(3, 4);
        let mut image2 = Image::new(3, 4);
        let data = vec![1; 2 * 3 * 3];

        image2
            .draw_raw_area(&data, ImageArea::new(0, 1, 2, 3))
            .unwrap();

        image.copy_updates(&image2).expect("Failed to copy updates");

        assert_eq!(image.get_data(), image2.get_data());
    }

    #[test]
    fn draw_raw_area() {
        let mut image = Image::new(3, 4);
        let data = vec![1; 2 * 3 * 3];
        image
            .draw_raw_area(&data, ImageArea::new(0, 1, 2, 3))
            .unwrap();
        assert_eq!(
            image.get_data(),
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0,
                1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0,
            ]
        );
        image
            .draw_raw_area(&data, ImageArea::new(1, 0, 2, 3))
            .unwrap();
        assert_eq!(
            image.get_data(),
            vec![
                0, 0, 0, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0,
                1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn draw_raw() {
        let width = 3;
        let height = 1;
        let mut image = Image::new(width, height);
        let data = vec![255; (3 * width * height).try_into().unwrap()];
        let _ = image
            .draw_raw(&data, 0, 0, width.into(), height.into())
            .unwrap();
        assert_eq!(
            image.get_data(),
            vec![255, 255, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0,]
        );
    }

    #[test]
    fn draw_raw_zlib() {
        use flate2::read::ZlibEncoder;
        use flate2::{Compress, Compression};

        let mut image = Image::new(3, 4);
        let data = vec![1; 2 * 3 * 3];
        let mut compressed = Vec::new();

        let mut z = ZlibEncoder::new_with_compress(
            data.as_slice(),
            Compress::new(Compression::new(1), false),
        );
        z.read_to_end(&mut compressed).unwrap();

        image
            .draw_raw_zlib(compressed.as_slice(), 0, 1, 2, 3)
            .expect("draw zlib");
        assert_eq!(
            image.get_data(),
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0,
                1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0,
            ]
        );
    }
}
