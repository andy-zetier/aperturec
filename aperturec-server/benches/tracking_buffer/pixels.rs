mod common;
use common::*;

use aperturec_graphics::prelude::*;
use aperturec_server::task::frame_sync::*;

use criterion::{measurement::*, *};
use ndarray::Zip;
use rand::random;
use std::hint::black_box;

struct UndamagedPixelsCaptured;
impl Measurement for UndamagedPixelsCaptured {
    type Intermediate = usize;
    type Value = usize;

    fn start(&self) -> Self::Intermediate {
        0
    }

    fn end(&self, i: Self::Intermediate) -> Self::Value {
        i
    }

    fn add(&self, v1: &Self::Value, v2: &Self::Value) -> Self::Value {
        *v1 + *v2
    }

    fn zero(&self) -> Self::Value {
        0
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        *value as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &UndamagedPixelsCapturedFormatter
    }
}

struct UndamagedPixelsCapturedFormatter;
impl ValueFormatter for UndamagedPixelsCapturedFormatter {
    fn scale_throughputs(&self, _: f64, _: &Throughput, _: &mut [f64]) -> &'static str {
        unimplemented!("UndamagedPixelsCaptured cannot be used with `Throughput`")
    }

    fn scale_values(&self, pixels: f64, values: &mut [f64]) -> &'static str {
        let (factor, unit) = if pixels < 1_000_f64 {
            (1_f64, "P")
        } else if pixels < 1_000_000_f64 {
            (1000_f64, "KP")
        } else {
            (1_000_000_f64, "MP")
        };

        for value in values {
            *value /= factor;
        }
        unit
    }

    fn scale_for_machines(&self, _: &mut [f64]) -> &'static str {
        "P"
    }
}

fn tracking_buffer_pixels(c: &mut Criterion<UndamagedPixelsCaptured>) {
    let mut group = c.benchmark_group("tb-undamaged-pixels-captured");

    for dim in DIMENSIONS {
        for num_areas in NUM_AREAS {
            group.bench_function(
                format!("{}-areas/{}x{}", num_areas, dim.width, dim.height),
                |bencher| {
                    bencher.iter_custom(|niters| {
                        black_box({
                            let mut undamaged_pixels_captured = 0;

                            for _ in 0..niters {
                                let mut curr =
                                    Pixel32Map::from_shape_fn(dim.as_shape(), |_| Pixel32 {
                                        red: random(),
                                        blue: random(),
                                        green: random(),
                                        alpha: u8::MAX,
                                    });
                                let mut new = curr.clone();
                                let updates = generate_updates(&curr, num_areas);

                                let mut tb = TrackingBuffer::new(
                                    0,
                                    to_display(dim),
                                    0,
                                    OutputPixelFormat::Bit24,
                                );
                                tb.update(&SubframeBuffer {
                                    origin: Point::zero(),
                                    pixels: curr.clone(),
                                });
                                let _ = tb.cut_frame();
                                for update in updates {
                                    tb.update(&update);
                                }
                                if let Some(frame) = tb.cut_frame() {
                                    let pixels_captured = frame
                                        .buffers
                                        .iter()
                                        .map(|buf| buf.area().area())
                                        .sum::<usize>();

                                    for buffer in frame.buffers {
                                        buffer.pixels.as_ndarray().assign_to(
                                            new.as_ndarray_mut()
                                                .slice_mut(buffer.area().as_slice()),
                                        );
                                    }
                                    let pixels_damaged =
                                        Zip::from(&new).and(&curr).fold(0, |acc, n, c| {
                                            if n != c { acc + 1 } else { acc }
                                        });
                                    undamaged_pixels_captured += pixels_captured - pixels_damaged;
                                }

                                curr.assign(&new);
                            }
                            if undamaged_pixels_captured == 0 {
                                // Criterion does not like measurements of 0 because it is meant to
                                // measure time. We are abusing it by measuring something other
                                // than time, where a 0 value actually makes sense. To satisfy
                                // Criterion, we just report 0 measurements as 1
                                1
                            } else {
                                undamaged_pixels_captured
                            }
                        })
                    })
                },
            );
        }
    }
}

fn undamaged_pixels_captured_measurement() -> Criterion<UndamagedPixelsCaptured> {
    Criterion::default().with_measurement(UndamagedPixelsCaptured)
}

criterion_group!(
    name = benches;
    config = undamaged_pixels_captured_measurement();
    targets = tracking_buffer_pixels
);
criterion_main!(benches);
