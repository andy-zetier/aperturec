use crate::prelude::*;

use image::{GrayImage, Luma};
use imageproc::region_labelling::{connected_components, Connectivity};
use ndarray::prelude::*;
use nshare::*;
use rayon::prelude::*;
use std::cmp::{max, min};
use std::collections::BTreeMap;

fn sampled_diff_image(
    a: impl PixelMap<Pixel = Pixel24> + Sync,
    b: impl PixelMap + Sync,
    sample_grid_size: usize,
) -> GrayImage {
    GrayImage::from_par_fn(
        (a.as_ndarray().len_of(axis::X) / sample_grid_size + 1) as u32,
        (a.as_ndarray().len_of(axis::Y) / sample_grid_size + 1) as u32,
        |x, y| {
            let (min_x, min_y) = (x as usize * sample_grid_size, y as usize * sample_grid_size);
            let max_x = min(min_x + sample_grid_size, a.as_ndarray().len_of(axis::X));
            let max_y = min(min_y + sample_grid_size, a.as_ndarray().len_of(axis::Y));
            if b.as_ndarray().slice(s![min_y..max_y, min_x..max_x])
                == a.as_ndarray().slice(s![min_y..max_y, min_x..max_x])
            {
                Luma([0_u8])
            } else {
                Luma([u8::MAX])
            }
        },
    )
}

fn region_bounds(labeled_regions: ArrayView2<u32>) -> BTreeMap<u32, Box2D> {
    fn merge_axis_bounds(
        mut a: BTreeMap<u32, (usize, usize)>,
        b: BTreeMap<u32, (usize, usize)>,
    ) -> BTreeMap<u32, (usize, usize)> {
        if a.is_empty() {
            b
        } else if b.is_empty() {
            a
        } else {
            b.into_iter().for_each(|(region, (b_min, b_max))| {
                a.entry(region)
                    .and_modify(|(a_min, a_max)| {
                        *a_min = min(*a_min, b_min);
                        *a_max = max(*a_max, b_max);
                    })
                    .or_insert((b_min, b_max));
            });
            a
        }
    }

    fn identify_bounds_1d(regions: ArrayView1<u32>) -> BTreeMap<u32, (usize, usize)> {
        regions
            .into_iter()
            .enumerate()
            .filter(|(_, &region)| region != 0)
            .fold(BTreeMap::default(), |mut curr_bounds, (idx, &region)| {
                curr_bounds
                    .entry(region)
                    .and_modify(|(curr_min, curr_max)| {
                        *curr_min = min(*curr_min, idx);
                        *curr_max = max(*curr_max, idx + 1);
                    })
                    .or_insert((idx, idx + 1));
                curr_bounds
            })
    }

    fn identify_axis_bounds(regions: ArrayView2<u32>, axis: Axis) -> BTreeMap<u32, (usize, usize)> {
        regions
            .axis_iter(axis)
            .into_par_iter()
            .map(identify_bounds_1d)
            .reduce(BTreeMap::default, merge_axis_bounds)
    }

    let (width_bounds, height_bounds) = rayon::join(
        || identify_axis_bounds(labeled_regions, axis::Y),
        || identify_axis_bounds(labeled_regions, axis::X),
    );

    assert_eq!(height_bounds.len(), width_bounds.len());
    height_bounds
        .into_iter()
        .zip(width_bounds.into_iter())
        .map(|((h_region, (top, bottom)), (w_region, (left, right)))| {
            assert_eq!(h_region, w_region);
            (
                h_region,
                Box2D::new(Point::new(left, top), Point::new(right, bottom)),
            )
        })
        .collect::<BTreeMap<_, _>>()
}

pub fn diff_rectangle_cover(
    a: impl PixelMap<Pixel = Pixel24> + Sync,
    b: impl PixelMap + Sync,
) -> BoxSet {
    const SAMPLE_GRID_SIZE: usize = 64;
    assert_eq!(a.as_ndarray().dim(), b.as_ndarray().dim());
    let size = a.size();

    let diff_image = sampled_diff_image(a, b, SAMPLE_GRID_SIZE);
    if diff_image.dimensions() == (1, 1) {
        return if diff_image.get_pixel(0, 0) == &Luma([0_u8]) {
            BoxSet::default()
        } else {
            BoxSet::with_initial_box(Box2D::from_size(size))
        };
    }
    let ccs = connected_components(&diff_image, Connectivity::Four, Luma([0_u8]));

    region_bounds(ccs.as_ndarray2())
        .into_values()
        .map(|mut b| {
            b.min.x *= SAMPLE_GRID_SIZE;
            b.min.y *= SAMPLE_GRID_SIZE;
            b.max.x *= SAMPLE_GRID_SIZE;
            b.max.y *= SAMPLE_GRID_SIZE;
            b.max.x = min(b.max.x, size.width);
            b.max.y = min(b.max.y, size.height);
            b
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn equal() {
        let size = Size::new(1920, 1080);
        let orig = Array2::from_elem(
            size.as_shape(),
            Pixel24 {
                red: u8::MAX,
                green: u8::MAX,
                blue: u8::MAX,
            },
        );
        let new = orig.clone();
        let diff = diff_rectangle_cover(orig.view(), new.view());
        assert!(diff.is_empty());
    }

    #[test]
    fn fully_different() {
        let size = Size::new(1920, 1080);
        let orig = Array2::from_elem(
            size.as_shape(),
            Pixel24 {
                red: u8::MAX,
                green: 0,
                blue: 0,
            },
        );
        let new = Array2::from_elem(
            size.as_shape(),
            Pixel24 {
                red: 0,
                green: u8::MAX,
                blue: 0,
            },
        );
        let mut diff = diff_rectangle_cover(orig.view(), new.view());
        assert_eq!(diff.len(), 1);
        assert_eq!(diff.pop().unwrap(), Box2D::from_size(size));
    }

    #[test]
    fn two_disjoint() {
        let size = Size::new(1920, 1080);
        let orig = Array2::from_elem(
            size.as_shape(),
            Pixel24 {
                red: u8::MAX,
                green: 0,
                blue: 0,
            },
        );
        let mut new = orig.clone();
        new.slice_mut(s![0..100, 0..100]).assign(&arr0(Pixel24 {
            red: 0,
            green: 0,
            blue: 0,
        }));
        new.slice_mut(s![200..300, 200..300]).assign(&arr0(Pixel24 {
            red: 0,
            green: 0,
            blue: 0,
        }));
        let diff = diff_rectangle_cover(orig.view(), new.view())
            .into_iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            &diff,
            &[
                Box2D::new(Point::new(0, 0), Point::new(128, 128)),
                Box2D::new(Point::new(192, 192), Point::new(320, 320))
            ]
        );
    }

    #[test]
    fn three_disjoint() {
        let size = Size::new(1920, 1080);
        let orig = Array2::from_elem(
            size.as_shape(),
            Pixel24 {
                red: u8::MAX,
                green: 0,
                blue: 0,
            },
        );
        let mut new = orig.clone();
        new.slice_mut(s![0..100, 0..100]).assign(&arr0(Pixel24 {
            red: 0,
            green: 0,
            blue: 0,
        }));
        new.slice_mut(s![200..300, 200..300]).assign(&arr0(Pixel24 {
            red: 0,
            green: 0,
            blue: 0,
        }));
        new.slice_mut(s![400..500, 400..500]).assign(&arr0(Pixel24 {
            red: 0,
            green: 0,
            blue: 0,
        }));
        let diff = diff_rectangle_cover(orig.view(), new.view())
            .into_iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            &diff,
            &[
                Box2D::new(Point::new(0, 0), Point::new(128, 128)),
                Box2D::new(Point::new(192, 192), Point::new(320, 320)),
                Box2D::new(Point::new(384, 384), Point::new(512, 512))
            ]
        );
    }
}
