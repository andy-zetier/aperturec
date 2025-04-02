mod common;
use common::*;

use aperturec_graphics::prelude::*;
use aperturec_server::task::frame_sync::*;

use criterion::*;
use ndarray::prelude::*;
use rand::random;
use std::time::{Duration, Instant};

fn tracking_buffer_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("tb-time");

    for dim in DIMENSIONS {
        for num_areas in NUM_AREAS {
            group.bench_function(
                format!("{}-areas/{}x{}", num_areas, dim.width, dim.height),
                |bencher| {
                    bencher.iter_custom(|niters| {
                        black_box({
                            let curr = Array2::from_shape_fn(dim.as_shape(), |_| Pixel24 {
                                red: random(),
                                blue: random(),
                                green: random(),
                            });
                            let mut duration = Duration::from_secs(0);
                            for _ in 0..niters {
                                let updates = generate_updates(&curr, num_areas);
                                let mut tb = TrackingBuffer::new(0, to_display(dim), 0);
                                let start = Instant::now();
                                for update in updates {
                                    tb.update(&update);
                                }
                                duration += start.elapsed();
                            }

                            duration
                        })
                    })
                },
            );
        }
    }
}

criterion_group!(benches, tracking_buffer_time);
criterion_main!(benches);
