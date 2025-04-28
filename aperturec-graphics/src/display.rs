use crate::geometry::*;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum Error {
    #[error("no displays are enabled")]
    NoEnabledDisplays,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct DisplayConfiguration {
    pub id: usize,
    pub display_decoder_infos: Box<[DisplayDecoderInfo]>,
}

impl DisplayConfiguration {
    pub fn encoder_count(&self) -> usize {
        self.display_decoder_infos
            .iter()
            .map(|ddi| ddi.decoder_areas.len())
            .sum()
    }
}

impl<'dc> IntoIterator for &'dc DisplayConfiguration {
    type Item = &'dc Display;
    type IntoIter = Box<dyn Iterator<Item = &'dc Display> + 'dc>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.display_decoder_infos.iter().map(|ddi| &ddi.display))
    }
}

#[derive(Debug, Clone)]
pub struct DisplayDecoderInfo {
    pub display: Display,
    pub decoder_areas: Box<[Rect]>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Display {
    pub area: Rect,
    pub is_enabled: bool,
}

impl Display {
    pub fn new(area: Rect, is_enabled: bool) -> Self {
        Self { area, is_enabled }
    }

    pub fn linear_displays(count: usize, size: Size) -> Vec<Self> {
        (0..count)
            .map(|i| {
                let origin = Point::new(i * size.width, 0);
                Self::new(Rect::new(origin, size), true)
            })
            .collect()
    }

    pub fn origin(&self) -> Point {
        self.area.origin
    }

    pub fn size(&self) -> Size {
        self.area.size
    }
}

pub trait DisplayExtent {
    fn derive_extent(self) -> Result<Size>;
}

impl<'d, I> DisplayExtent for I
where
    I: IntoIterator<Item = &'d Display>,
{
    fn derive_extent(self) -> Result<Size> {
        let mut has_origin_display = false;

        let extent = self
            .into_iter()
            .filter(|d| d.is_enabled)
            .inspect(|d| {
                if d.area.origin == Point::zero() {
                    has_origin_display = true
                }
            })
            .map(|display| display.area)
            .extent();

        let Some(extent) = extent else {
            return Err(Error::NoEnabledDisplays);
        };

        let b = extent.to_box2d();
        Ok(Size::new(b.max.x, b.max.y))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn linear_displays() {
        let size = Size::new(1920, 1080);
        let displays = Display::linear_displays(3, size);

        // Ensure the correct number of displays were created
        assert_eq!(displays.len(), 3);

        for (i, display) in displays.iter().enumerate() {
            // Check position (x should be contiguous, y should remain 0)
            assert_eq!(display.origin().x, i * size.width);
            assert_eq!(display.origin().y, 0);

            // Ensure all displays have the same size
            assert_eq!(display.size(), size);

            // Ensure displays are enabled by default
            assert!(display.is_enabled);
        }

        // Verify that displays are contiguous
        assert_eq!(displays[0].origin().x, 0);
        assert_eq!(displays[1].origin().x, 1920);
        assert_eq!(displays[2].origin().x, 3840);
    }

    #[test]
    fn derive_display_extent() {
        let cases = vec![
            (
                "Single display at origin",
                vec![Display {
                    area: Rect::new(Point::zero(), Size::new(1920, 1080)),
                    is_enabled: true,
                }],
                Ok(Size::new(1920, 1080)),
            ),
            (
                "Two non-overlapping displays",
                vec![
                    Display {
                        area: Rect::new(Point::zero(), Size::new(1920, 1080)),
                        is_enabled: true,
                    },
                    Display {
                        area: Rect::new(Point::new(1920, 0), Size::new(1280, 1080)),
                        is_enabled: true,
                    },
                ],
                Ok(Size::new(3200, 1080)),
            ),
            (
                "Overlapping displays",
                vec![
                    Display {
                        area: Rect::new(Point::zero(), Size::new(1920, 1080)),
                        is_enabled: true,
                    },
                    Display {
                        area: Rect::new(Point::new(1600, 0), Size::new(1280, 1080)),
                        is_enabled: true,
                    },
                ],
                Ok(Size::new(2880, 1080)),
            ),
            (
                "Disabled displays are ignored",
                vec![
                    Display {
                        area: Rect::new(Point::zero(), Size::new(1920, 1080)),
                        is_enabled: true,
                    },
                    Display {
                        area: Rect::new(Point::new(1920, 0), Size::new(1920, 1080)),
                        is_enabled: false, // Should be ignored
                    },
                ],
                Ok(Size::new(1920, 1080)),
            ),
            (
                "No enabled displays returns error",
                vec![
                    Display {
                        area: Rect::new(Point::zero(), Size::new(1920, 1080)),
                        is_enabled: false,
                    },
                    Display {
                        area: Rect::new(Point::new(1920, 0), Size::new(1280, 1080)),
                        is_enabled: false,
                    },
                ],
                Err(Error::NoEnabledDisplays),
            ),
        ];

        for (desc, displays, expected) in cases {
            let result = displays.derive_extent();
            match expected {
                Ok(expected_size) => {
                    assert!(
                        result.is_ok(),
                        "{}: Expected Ok({:?}), but got Err({:?})",
                        desc,
                        expected_size,
                        result.err()
                    );
                    assert_eq!(
                        result.unwrap(),
                        expected_size,
                        "{}: Display extent size mismatch",
                        desc
                    );
                }
                Err(expected_err) => {
                    assert!(
                        result.is_err(),
                        "{}: Expected Err({}), but got Ok({:?})",
                        desc,
                        expected_err,
                        result.ok()
                    );
                    assert_eq!(
                        result.unwrap_err(),
                        expected_err,
                        "{}: Error message mismatch",
                        desc
                    );
                }
            }
        }
    }
}
