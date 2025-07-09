use aperturec_graphics::{display::*, prelude::*};
use aperturec_server::task::frame_sync::*;

use ndarray::{Zip, prelude::*};
use rand::{distributions::*, prelude::*};
use std::ops::RangeInclusive;

pub fn to_display(dim: Size) -> Display {
    Display {
        area: Rect::from_size(dim),
        is_enabled: true,
    }
}

pub const DIMENSIONS: [Size; 4] = [
    Size::new(800, 600),
    Size::new(1920, 1080),
    Size::new(2560, 1440),
    Size::new(3840, 2160),
];

pub const NUM_AREAS: RangeInclusive<usize> = 1..=8;

pub const MIN_LEN: usize = 8;

enum DamageShape {
    Top,         // =  ¯
    TopLeft,     // = |¯
    Left,        // = |
    BottomLeft,  // = |_
    Bottom,      // =  _
    BottomRight, // =  _|
    Right,       // =   |
    TopRight,    // =  ¯|
}

impl Distribution<DamageShape> for Standard {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> DamageShape {
        match rng.gen_range(0..8) {
            0 => DamageShape::Top,
            1 => DamageShape::TopLeft,
            2 => DamageShape::Left,
            3 => DamageShape::BottomLeft,
            4 => DamageShape::Bottom,
            5 => DamageShape::BottomRight,
            6 => DamageShape::Right,
            _ => DamageShape::TopRight,
        }
    }
}

pub fn generate_updates(curr: &Pixel32Map, num_areas: usize) -> Vec<SubframeBuffer<Pixel32Map>> {
    let mut rng = rand::thread_rng();
    let total_area = Size::new(
        curr.as_ndarray().len_of(axis::X),
        curr.as_ndarray().len_of(axis::Y),
    );

    (0..num_areas)
        .map(|_| {
            let origin = Point::new(
                rng.gen_range(0..total_area.width - MIN_LEN),
                rng.gen_range(0..total_area.height - MIN_LEN),
            );
            let max_width = total_area.width - origin.x;
            let max_height = total_area.height - origin.y;
            let size = Size::new(
                rng.gen_range(MIN_LEN..=max_width),
                rng.gen_range(MIN_LEN..=max_height),
            );
            let area = Rect::new(origin, size).to_box2d();

            let curr = curr.as_ndarray().slice(area.as_slice()).to_owned();
            let mut new = Array2::from_shape_fn(area.as_shape(), |_| Pixel32 {
                red: random(),
                blue: random(),
                green: random(),
                alpha: u8::MAX,
            });

            let damaged_width = rng.gen_range(0..=area.width());
            let damaged_height = rng.gen_range(0..=area.height());
            let damage_shape = random::<DamageShape>();

            Zip::indexed(&mut new).and(&curr).for_each(|(x, y), n, c| {
                let do_assign = match damage_shape {
                    DamageShape::Top => y < damaged_height,
                    DamageShape::TopLeft => y < damaged_height || x < damaged_width,
                    DamageShape::Left => x < damaged_width,
                    DamageShape::BottomLeft => {
                        y > area.height() - damaged_height || x < damaged_width
                    }
                    DamageShape::Bottom => y > area.height() - damaged_height,
                    DamageShape::BottomRight => {
                        y > area.height() - damaged_height || x > area.width() - damaged_width
                    }
                    DamageShape::Right => x > area.width() - damaged_width,
                    DamageShape::TopRight => x > area.width() - damaged_width || y < damaged_height,
                };

                if !do_assign {
                    *n = *c;
                }
            });

            SubframeBuffer {
                origin: area.min,
                pixels: new,
            }
        })
        .collect()
}
