use crate::geometry::*;

use std::cmp::{max, min};
use std::collections::VecDeque;

#[derive(Copy, Clone)]
struct FactorPair {
    w: usize,
    h: usize,
}

impl FactorPair {
    fn update<T>(&mut self, width: T, height: T)
    where
        T: Into<usize>,
    {
        self.w = width.into();
        self.h = height.into();
    }
}

pub fn partition(client_resolution: &Size, max_decoder_count: usize) -> (Size, Vec<Box2D>) {
    let n_encoders = if max_decoder_count > 1 {
        max_decoder_count / 2 * 2
    } else {
        max_decoder_count
    };

    let width = client_resolution.width;
    let height = client_resolution.height;

    //
    // Gather a list of factor pairs for the given n_encoders arranged from largest aspect
    // ratio to the least.
    //
    let mut factors: VecDeque<_> = (1..=n_encoders).filter(|&x| n_encoders % x == 0).collect();

    let mut factor_pairs: Vec<FactorPair> = vec![
        FactorPair { w: 0, h: 0 };
        (if factors.len() & 0x1 == 1 {
            factors.len() + 1
        } else {
            factors.len()
        }) / 2
    ];

    for fp in factor_pairs.iter_mut() {
        let f0 = factors.pop_front().expect("pop front");
        if !factors.is_empty() {
            let f1 = factors.pop_back().expect("pop back");
            if width >= height {
                fp.update(max(f0, f1), min(f0, f1));
            } else {
                fp.update(min(f0, f1), max(f0, f1));
            }
        } else {
            fp.update(f0, f0);
        }
    }

    if !factors.is_empty() {
        panic!(
            "{} factors of {} remain! {:#?}",
            factors.len(),
            n_encoders,
            factors
        );
    }

    if factor_pairs.is_empty() {
        panic!("{} {}x{} has no factor pairs!", n_encoders, width, height);
    }

    //
    // Define a series of tests for the FactorPairs, in descending order, of how well a
    // FactorPair divides the requested width and height.
    //
    type FactorTest<'a> = dyn Fn(&FactorPair) -> bool + 'a;
    let tests: Vec<Box<FactorTest>> = vec![
        Box::new(|fp| width % fp.w == 0 && height % fp.h == 0),
        Box::new(|fp| width % fp.w == 1 && height % fp.h == 0),
        Box::new(|fp| width % fp.w == 0 && height % fp.h == 1),
        Box::new(|fp| width % fp.w == 0 || height % fp.h == 0),
    ];

    //
    // Evaluate each test, from best to worst, against the FactorPairs which are arranged in
    // increasing aspect ratio. Break when we find the best fit. Note the last test and the
    // FactorPair with the highest aspect ratio form a base case since 1 divides anything.
    //
    let mut best_fit = factor_pairs[0];
    'outer: for test in tests.iter() {
        for fp in factor_pairs.iter().rev() {
            if test(fp) {
                best_fit = *fp;
                break 'outer;
            }
        }
    }

    let sz = Size::new(width / best_fit.w, height / best_fit.h);

    let mut partitions = vec![
        Rect {
            size: sz,
            origin: euclid::Point2D::new(0, 0)
        };
        best_fit.h * best_fit.w
    ];

    let mut i = 0;
    for y in 0..best_fit.h {
        for x in 0..best_fit.w {
            partitions[i].origin.x = x * sz.width;
            partitions[i].origin.y = y * sz.height;
            i += 1;
        }
    }

    let server_width = partitions
        .iter()
        .max_by_key(|rect| rect.origin.x)
        .map(|rect| rect.origin.x)
        .expect("partition server width")
        + partitions[0].size.width;
    let server_height = partitions
        .iter()
        .max_by_key(|rect| rect.origin.y)
        .map(|rect| rect.origin.y)
        .expect("partition server width")
        + partitions[0].size.height;
    let server_resolution = Size::new(server_width, server_height);
    (
        server_resolution,
        partitions.iter().map(|p| p.to_box2d()).collect(),
    )
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn partitions() {
        let mut dims = vec![];

        dims.push((Size::new(800, 600), 8));
        dims.push((Size::new(1470, 956), 8));
        dims.push((Size::new(813, 600), 32));

        for d in dims.iter() {
            let (_, partitions) = partition(&d.0, d.1);
            for p in partitions.iter() {
                assert!(
                    p.min.x == 0 || (p.min.x % p.width()) == 0,
                    "Invalid x/width {:#?}",
                    p,
                );
                assert!(
                    p.min.y == 0 || (p.min.y % p.height()) == 0,
                    "Invalid y/height {:#?}",
                    p,
                );
            }
        }
    }
}
