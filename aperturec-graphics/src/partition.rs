use crate::display::Display;
use crate::prelude::*;
use std::num::NonZeroUsize;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("enabled display count {} exceeds max decoders {}", .display_count, .max_decoders)]
    MoreDisplaysThanDecoders {
        display_count: usize,
        max_decoders: usize,
    },
    #[error("no displays are enabled")]
    NoEnabledDisplays,
    #[error("max decoder count is zero")]
    MaxDecoderCountZero,
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn partition_displays(
    displays: &[Display],
    max_decoder_count: usize,
) -> Result<Vec<Vec<Box2D>>> {
    let enabled = displays.iter().filter(|d| d.is_enabled).count();
    if enabled == 0 {
        return Err(Error::NoEnabledDisplays);
    }
    if max_decoder_count == 0 {
        return Err(Error::MaxDecoderCountZero);
    }
    if enabled > max_decoder_count {
        return Err(Error::MoreDisplaysThanDecoders {
            display_count: enabled,
            max_decoders: max_decoder_count,
        });
    }

    let base = max_decoder_count / enabled;
    let mut rem = max_decoder_count % enabled;
    Ok(displays
        .iter()
        .map(|d| {
            if !d.is_enabled {
                Vec::new()
            } else {
                let extra = if rem > 0 {
                    rem -= 1;
                    1
                } else {
                    0
                };
                let n = NonZeroUsize::new(base + extra).unwrap();
                partition(d.size(), n)
            }
        })
        .collect())
}

struct Layout {
    rows: usize,
    cols: usize,
    unused: usize,
    squareness: usize,
}

impl Layout {
    fn score(&self) -> usize {
        // Favor 0 unused over the squareness measure
        self.unused * 10 + self.squareness
    }
}

pub fn partition(resolution: Size, max_partitions: NonZeroUsize) -> Vec<Box2D> {
    let max_partitions = max_partitions.get();
    let width = resolution.width;
    let height = resolution.height;

    let mut best: Option<Layout> = None;

    //
    // Select the most "grid-like" paritioning layout given max_partitions. Prioritize layouts that
    // use exactly max_partitions (no unused) and prefer layouts where rows are roughly equal to
    // columns. Note dividing by 1 forms a base case. Lowest score wins.
    //
    for rows in 1..=max_partitions {
        let cols = max_partitions.div_ceil(rows);
        let total = rows * cols;
        if total < max_partitions {
            continue;
        }

        let layout = Layout {
            rows,
            cols,
            unused: total - max_partitions,
            squareness: (rows as isize - cols as isize).unsigned_abs(),
        };

        if best.as_ref().is_none_or(|b| layout.score() < b.score()) {
            best = Some(layout);
        }
    }

    let layout = best.expect("best layout");
    let mut partitions = Vec::with_capacity(max_partitions);

    'outer: for row in 0..layout.rows {
        let y0 = height * row / layout.rows;
        let y1 = height * (row + 1) / layout.rows;

        for col in 0..layout.cols {
            let x0 = width * col / layout.cols;
            let x1 = width * (col + 1) / layout.cols;

            partitions.push(Box2D {
                min: Point::new(x0, y0),
                max: Point::new(x1, y1),
            });

            if partitions.len() == max_partitions {
                break 'outer;
            }
        }
    }

    partitions
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn all_decoders_used() {
        let max_decoders = 32;
        let size = Size::new(1920, 1080);

        for display_cnt in 1..=max_decoders {
            let displays = Display::linear_displays(display_cnt, size);

            for decoder_cnt in display_cnt..=max_decoders {
                let partitions =
                    partition_displays(&displays, decoder_cnt).expect("partition failed");
                let used: usize = partitions.iter().map(|v| v.len()).sum();
                assert_eq!(
                    used, decoder_cnt,
                    "expected all {decoder_cnt} decoders to be used with {display_cnt} displays"
                );
            }
        }
    }

    #[test]
    fn partitioning() {
        let test_cases = vec![
            (Size::new(800, 600), NonZeroUsize::new(8).unwrap()),
            (Size::new(1470, 956), NonZeroUsize::new(8).unwrap()),
            (Size::new(100, 1000), NonZeroUsize::new(3).unwrap()),
            (Size::new(123, 456), NonZeroUsize::new(1).unwrap()),
            (Size::new(500, 500), NonZeroUsize::new(5).unwrap()),
            (Size::new(20, 20), NonZeroUsize::new(7).unwrap()),
            (Size::new(1470, 956), NonZeroUsize::new(8).unwrap()),
            (Size::new(813, 600), NonZeroUsize::new(32).unwrap()),
            (Size::new(2560, 1440), NonZeroUsize::new(6).unwrap()),
            (Size::new(1920, 1200), NonZeroUsize::new(6).unwrap()),
        ];

        for (resolution, count) in test_cases {
            let partitions = partition(resolution, count);
            let max_partitions = count.get();

            assert_eq!(
                partitions.len(),
                max_partitions,
                "Expected {} partitions, got {}",
                max_partitions,
                partitions.len()
            );

            let total_area: usize = partitions
                .iter()
                .map(|p| (p.max.x - p.min.x) * (p.max.y - p.min.y))
                .sum();

            assert_eq!(
                total_area,
                resolution.width * resolution.height,
                "Total area mismatch for resolution {:?}",
                resolution
            );
        }

        let pixel_coverage_test_cases = vec![
            (Size::new(20, 20), NonZeroUsize::new(7).unwrap()),
            (Size::new(100, 105), NonZeroUsize::new(12).unwrap()),
            (Size::new(75, 4), NonZeroUsize::new(33).unwrap()),
        ];

        for (resolution, count) in pixel_coverage_test_cases {
            let partitions = partition(resolution, count);

            let mut pixel_map = vec![vec![0u8; resolution.width]; resolution.height];

            partitions.iter().for_each(|p| {
                (p.min.y..p.max.y).for_each(|y| {
                    (p.min.x..p.max.x).for_each(|x| {
                        pixel_map[y][x] += 1;
                    });
                });
            });

            for (y, row) in pixel_map.iter().enumerate() {
                for (x, count) in row.iter().enumerate() {
                    assert_eq!(
                        *count, 1,
                        "Pixel ({}, {}) was covered {} times (expected exactly once)",
                        x, y, count
                    );
                }
            }
        }
    }
}
