use aperturec_graphics::prelude::{Pixel32, Point};
use ndarray::ArcArray2;

/// Cursor appearance and hotspot information.
///
/// Represents a cursor image with its pixel data and hotspot position.
/// The hotspot is the point within the cursor image that corresponds to
/// the actual pointer position.
#[derive(Clone, Debug)]
pub struct Cursor {
    /// Hotspot position within the cursor image.
    ///
    /// This is the point that corresponds to the actual mouse pointer position.
    pub hot: Point,
    /// Cursor pixel data in 32-bit RGBA format.
    ///
    /// The cursor image is stored as a 2D array of RGBA pixels.
    pub pixels: ArcArray2<Pixel32>,
}
