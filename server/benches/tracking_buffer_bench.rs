use aperturec_server::task::encoder::{Point, RawPixel, Size, TrackingBuffer};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use ndarray::Array2;
use rand::Rng;

trait RandomExt {
    fn new_random() -> Self;
}

impl RandomExt for RawPixel {
    // Function to generate a random RawPixel
    fn new_random() -> Self {
        let mut rng = rand::thread_rng();
        RawPixel {
            red: rng.gen(),
            green: rng.gen(),
            blue: rng.gen(),
            alpha: rng.gen(),
        }
    }
}

// Function to simulate the update method
fn update_tracking_buffer(buffer: &mut TrackingBuffer, new_data: Array2<RawPixel>) {
    buffer.update(Point::new(0, 0), new_data.view());
}

// Benchmarking function
fn benchmark_update(c: &mut Criterion) {
    c.bench_function("tracking_buffer_update_small", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(800, 600));
        b.iter_batched(
            || Array2::from_elem((100, 100), RawPixel::new_random()),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("tracking_buffer_update_large", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(1920, 1080));
        b.iter_batched(
            || Array2::from_elem((1000, 1000), RawPixel::new_random()),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("tracking_buffer_update_massive", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(2560, 1440));
        b.iter_batched(
            || Array2::from_elem((2550, 1439), RawPixel::new_random()),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("tracking_buffer_update_small_fake", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(800, 600));
        let new_data = Array2::from_elem((100, 100), RawPixel::new_random());
        buffer.update(Point::new(0, 0), new_data.view());
        b.iter_batched(
            || new_data.clone(),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("tracking_buffer_update_large_fake", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(1920, 1080));
        let new_data = Array2::from_elem((1000, 1000), RawPixel::new_random());
        buffer.update(Point::new(0, 0), new_data.view());
        b.iter_batched(
            || new_data.clone(),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("tracking_buffer_update_massive_fake", |b| {
        let mut buffer = TrackingBuffer::new(Size::new(2560, 1440));
        let new_data = Array2::from_elem((2550, 1439), RawPixel::new_random());
        buffer.update(Point::new(0, 0), new_data.view());
        b.iter_batched(
            || new_data.clone(),
            |new_data| update_tracking_buffer(black_box(&mut buffer), black_box(new_data)),
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, benchmark_update);
criterion_main!(benches);
