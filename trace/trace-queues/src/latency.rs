use anyhow::Result;
use std::collections::VecDeque;
use std::path::Path;

pub(crate) fn analyze_latency<'p, R: AsRef<Path>>(
    paths: impl IntoIterator<Item = R>,
) -> Result<()> {
    let mut histogram = hdrhistogram::Histogram::<u64>::new(3)?;
    for input_file in paths {
        let reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(input_file)?;
        let points = reader
            .into_deserialize()
            .collect::<Result<Vec<(u64, u64)>, _>>()?;

        let mut in_flight = VecDeque::new();
        let mut prev_items_in_queue = 0;
        for (timestamp, items_in_queue) in points {
            if prev_items_in_queue < items_in_queue {
                for _ in prev_items_in_queue..items_in_queue {
                    in_flight.push_back(timestamp);
                }
            } else if prev_items_in_queue > items_in_queue {
                let deq_time = timestamp;
                for _ in items_in_queue..prev_items_in_queue {
                    let enq_time = in_flight.pop_front().unwrap();

                    histogram.record(deq_time - enq_time)?;
                }
            }
            prev_items_in_queue = items_in_queue;
        }
    }

    let p50 = histogram.value_at_quantile(0.5);
    let p75 = histogram.value_at_quantile(0.75);
    let p90 = histogram.value_at_quantile(0.9);
    let p95 = histogram.value_at_quantile(0.95);
    let p99 = histogram.value_at_quantile(0.99);
    let p999 = histogram.value_at_quantile(0.999);

    println!("P50:  {}ns = {:03}us", p50, p50 as f64 / 1_000.0);
    println!("P75:  {}ns = {:03}us", p75, p75 as f64 / 1_000.0);
    println!("P90:  {}ns = {:03}us", p90, p90 as f64 / 1_000.0);
    println!("P95:  {}ns = {:03}us", p95, p95 as f64 / 1_000.0);
    println!("P99:  {}ns = {:03}us", p99, p99 as f64 / 1_000.0);
    println!("P999: {}ns = {:03}us", p999, p999 as f64 / 1_000.0);

    Ok(())
}
