//! Macros for easily creating Metrics
//!
//! The macros included in this module can be used to easily create and register various types of
//! Metrics with the Metrics system. For example:
//!
//! ```no_run
//! use aperturec_trace::Level;
//! use aperturec_quic_metrics::MetricsInitializer;
//! use aperturec_quic_metrics::exporters::{Exporter, LogExporter, PushgatewayExporter};
//!
//! // Create a metric
//! aperturec_quic_metrics::create_metric!(MyMetric, "label");
//!
//! // Create a histogram metric
//! aperturec_quic_metrics::create_histogram_metric!(MyHistogram);
//!
//! // Create a stats metric
//! aperturec_quic_metrics::create_stats_metric!(MyStatsMetric);
//!
//! fn main() {
//!     MetricsInitializer::default()
//!         .with_exporter(Exporter::Log(LogExporter::new(Level::DEBUG).unwrap()))
//!         .with_exporter(Exporter::Pushgateway(PushgatewayExporter::new(
//!             String::from("http://127.0.0.1:9091"),
//!             String::from("my job"),
//!             1234
//!         ).unwrap()))
//!         .init()
//!         .expect("Failed to init metrics");
//!
//!     // Register the macro-created metrics
//!     MyMetric::register();
//!     MyHistogram::register();
//!     MyStatsMetric::register_sticky();
//!
//!     // Increment the "MyMetric" metric
//!     MyMetric::inc();
//!
//!     // Add an observation to "MyHistogram"
//!     MyHistogram::observe(123.4);
//!
//!     // Add a computation result to "MyStatsMetric"
//!     let data = 1.0;
//!     MyStatsMetric::update_with(move || (data + 7.0) / 4.0);
//!
//!     aperturec_quic_metrics::stop();
//! }
//!```

///
/// Create a [`Metric`](crate::Metric).
///
/// This macro will generate the required structs and trait implementations to define a
/// [`Metric`](crate::Metric) that can be updated, incremented, or decremented. The
/// `register()` associated function must be called to register this generated
/// [`Metric`](crate::Metric) for tracking.
///
/// The generated metric includes associated functions to modify the metric.
/// Presuming a metric name of `MyMetric`, the following functions are available:
///
/// * `MyMetric::update(new_value)` Sets the metric value to `new_value`
/// * `MyMetric::inc()` Increments the metric by 1
/// * `MyMetric::dec()` Decrements the metric by 1
/// * `MyMetric::inc_by(count)` Increments the metric by `count`
/// * `MyMetric::dec_by(count)` Decrements the metric by `count`
/// * `MyMetric::register()` Registers the Metric. Must be called prior to use.
///
#[macro_export]
macro_rules! create_metric {
    ($name:ty, $label:literal) => {
        paste::paste! {
            #[derive(Debug, Default, PartialEq)]
            pub struct [<$name:camel>] {
                data: f64,
            }

            enum [<$name:camel "Update">] {
                Update(f64),
                Increment(f64),
                Decrement(f64),
            }

            #[allow(dead_code)]
            impl [<$name:camel>] {
                pub fn update(data: impl Into<f64>) {
                    $crate::update([<$name:camel "Update">]::Update(data.into()));
                }

                pub fn inc() {
                    $crate::update([<$name:camel "Update">]::Increment(1.0));
                }

                pub fn dec() {
                    $crate::update([<$name:camel "Update">]::Decrement(1.0));
                }

                pub fn inc_by(data: impl Into<f64>) {
                    $crate::update([<$name:camel "Update">]::Increment(data.into()));
                }

                pub fn dec_by(data: impl Into<f64>) {
                    $crate::update([<$name:camel "Update">]::Decrement(data.into()));
                }

                pub fn register() {
                    $crate::register_default_metric!(Self);
                }
            }

            impl $crate::MetricUpdate for [<$name:camel "Update">] {}

            impl $crate::Metric for [<$name:camel>] {
                fn poll(&self) -> Vec<$crate::Measurement> {
                    vec![$crate::Measurement::new(
                        stringify!($name),
                        Some(self.data),
                        $label,
                        "",
                    )]
                }

                fn update(&mut self, update: Box<dyn std::any::Any>) {
                    if let Ok(mmu) = update.downcast::<[<$name:camel "Update">]>() {
                        match *mmu {
                            [<$name:camel "Update">]::Update(d) => self.data = d,
                            [<$name:camel "Update">]::Increment(d) => self.data += d,
                            [<$name:camel "Update">]::Decrement(d) => self.data -= d,
                        }
                    }
                }

                fn get_update_type_id(&self) -> std::any::TypeId {
                    std::any::TypeId::of::<[<$name:camel "Update">]>()
                }
            }
        }
    };

    ($name:ty) => {
        paste::paste! {
            $crate::create_metric!($name, "");
        }
    };
}

///
/// Create a Stats [`Metric`](crate::Metric).
///
/// This macro will generate the required structs and trait implementations to define a
/// [`Metric`](crate::Metric) that will generate statistical data for the values that have been
/// recorded since the last `poll()`. The
/// `register()` associated function must be called to register this generated
/// [`Metric`](crate::Metric) for tracking.
///
/// The generated metric includes associated functions to modify the metric.
/// Presuming a metric name of `MyMetric`, the following functions are available:
///
/// * `MyMetric::update_with(FnOnce)` Sets the metric value to the value returned by the closure
/// * `MyMetric::update_last(FnOnce(f64))` Provides the last value to a closure and stores the
/// result
/// * `MyMetric::update(new_value)` Sets the metric value to `new_value`
/// * `MyMetric::inc()` Increments the metric by 1
/// * `MyMetric::dec()` Decrements the metric by 1
/// * `MyMetric::inc_by(count)` Increments the metric by `count`
/// * `MyMetric::dec_by(count)` Decrements the metric by `count`
/// * `MyMetric::register()` Registers the Metric. Must be called prior to use.
/// * `MyMetric::register_sticky()` Registers the Metric. This version of a Stats Metric will
/// retain the last value across `poll()` calls instead of being reset to 0.
///
/// Statistical information published by this Metric includes:
/// * Average
/// * Max
/// * Min
/// * Variance
/// * Last - The most recent value
///
#[macro_export]
macro_rules! create_stats_metric {
    ($name:ty, $label:literal, $scale:literal) => {
        paste::paste! {
            #[derive(Debug, Default, PartialEq)]
            pub struct [<$name:camel>] {
                data: Vec<f64>,
                last_data: Vec<f64>,
                is_sticky: bool,
            }

            enum [<$name:camel "Update">] {
                Update(Box<dyn FnOnce() -> f64 + Send>),
                UpdateLast(Box<dyn FnOnce(f64) -> f64 + Send>),
            }

            #[allow(dead_code)]
            impl [<$name:camel>] {
                pub fn update_with(f: impl FnOnce() -> f64 + Send + 'static) {
                    $crate::update([<$name:camel "Update">]::Update(Box::new(f)));
                }

                pub fn update_last(f: impl FnOnce(f64) -> f64 + Send + 'static) {
                    $crate::update([<$name:camel "Update">]::UpdateLast(Box::new(f)));
                }

                pub fn update(data: impl Into<f64>) {
                    let data = data.into();
                    Self::update_with(move || {data});
                }

                pub fn inc() {
                    Self::inc_by(1.0);
                }

                pub fn dec() {
                    Self::dec_by(1.0);
                }

                pub fn inc_by(data: impl Into<f64>) {
                    let data = data.into();
                    Self::update_last(move |last| { last + data });
                }

                pub fn dec_by(data: impl Into<f64>) {
                    let data = data.into();
                    Self::update_last(move |last| { last - data });
                }

                pub fn register() {
                    $crate::register_default_metric!(Self);
                }

                pub fn register_sticky() {
                    let mut this = Self::default();
                    this.is_sticky = true;
                    $crate::register(move || Box::<Self>::new(this));
                }
            }

            impl $crate::MetricUpdate for [<$name:camel "Update">] {}

            impl $crate::Metric for [<$name:camel>] {
                fn poll(&self) -> Vec<$crate::Measurement> {
                    let scale = $scale;

                    let d = if self.is_sticky && self.data.is_empty() {
                        &self.last_data
                    } else {
                        &self.data
                    };

                    let mut period_avg = None;
                    let mut period_max = None;
                    let mut period_min = None;
                    let mut period_var = None;
                    let mut period_last = None;

                    if !d.is_empty() {
                        let sum = d.iter().sum::<f64>();
                        let count = d.len() as f64;
                        let avg = sum / count;
                        let max = d.iter().max_by(|a, b| a.total_cmp(b)).unwrap();
                        let min = d.iter().min_by(|a, b| a.total_cmp(b)).unwrap();
                        let var = d.iter().map(|m| {
                            let diff = avg - m;
                            diff * diff
                        })
                        .sum::<f64>()
                        / count;

                        period_avg = Some(avg * scale);
                        period_max = Some(max * scale);
                        period_min = Some(min * scale);
                        period_var = Some(var * scale);
                        period_last = Some(d.last().unwrap_or(&0_f64) * scale);
                    }

                    vec![
                        $crate::Measurement::new(stringify!($name), period_last, $label, ""),
                        $crate::Measurement::new(concat!(stringify!($name), " Avg"), period_avg, $label, ""),
                        $crate::Measurement::new(concat!(stringify!($name), " Max"), period_max, $label, ""),
                        $crate::Measurement::new(concat!(stringify!($name), " Min"), period_min, $label, ""),
                        $crate::Measurement::new(concat!(stringify!($name), " Var"), period_var, $label, ""),
                    ]
                }

                fn reset(&mut self) {
                    if ! self.data.is_empty() {
                        if self.is_sticky {
                            self.last_data = self.data.clone();
                        }
                        self.data.clear();
                    }
                }

                fn update(&mut self, update: Box<dyn std::any::Any>) {
                    if let Ok(m) = update.downcast::<[<$name:camel "Update">]>() {
                        match *m {
                            [<$name:camel "Update">]::Update(f) => self.data.push(f()),
                            [<$name:camel "Update">]::UpdateLast(f) => {
                                let last = self.data.last().unwrap_or(&0.0);
                                self.data.push(f(last.clone()));
                            },
                        }
                    }
                }

                fn get_update_type_id(&self) -> std::any::TypeId {
                    std::any::TypeId::of::<[<$name:camel "Update">]>()
                }
            }
        }
    };

    ($name:ty, $label: literal) => {
        paste::paste! {
            $crate::create_stats_metric!($name, $label, 1.0);
        }
    };

    ($name:ty) => {
        paste::paste! {
            $crate::create_stats_metric!($name, "", 1.0);
        }
    };
}

///
/// Create a [`Metric`](crate::Metric) which wraps a [`prometheus::Histogram`](prometheus::Histogram).
///
/// This macro will generate the required structs and trait implementations to define a
/// [`Histogram`](prometheus::Histogram) managed by the metrics system. This allows you to call the
/// [`observe()`](prometheus::Histogram::observe) method without requiring a reference to the
/// underlying [`Histogram`](prometheus::Histogram). As with
/// [`create_metric`](crate::create_metric), you will need to call
/// `register()` to setup tracking.
///
/// Histogram metric data is most useful when analyzed with Prometheus and Grafana. Simple averages
/// are generated and displayed for text based [`Exporters`](crate::exporters) such as
/// `LogExporter` and `CsvExporter`.
///
/// The generated metric will include associated functions to modify the metric.
/// Presuming a metric name of `MyHistogram`, the following functions are available:
///
/// * `MyHistogram::observe(value)` Calls [`observe()`](prometheus::Histogram::observe) on the
/// generated histogram metric
///
/// If you need access to the full set of `Histogram` features, the raw prometheus macros such as
/// [`register_histogram`](prometheus::register_histogram) can be used with the
/// [`PushgatewayExporter`](crate::exporters::PushgatewayExporter). However, this data will not be
/// available to any of the other exporters.
///
#[macro_export]
macro_rules! create_histogram_metric {
    ($name:ty, $label: literal, $opts: expr) => {
        paste::paste! {
            #[derive(Debug)]
            pub struct [<$name:camel>] {
                measurement_title: String,
                histogram: prometheus::Histogram,
            }

            impl Default for [<$name:camel>] {
                fn default() -> Self {
                    let measurement_title = stringify!([<$name:camel Hist>])
                        .chars()
                        .map(|c| if c.is_uppercase() {format!(" {}", c)} else {String::from(c)})
                        .collect::<String>()
                        .trim_start()
                        .to_string();
                    let hopts = $opts;
                    let hist = prometheus::Histogram::with_opts(hopts).unwrap();
                    Self {
                        measurement_title,
                        histogram: prometheus::register(Box::new(hist.clone())).map(|_| hist).unwrap(),
                    }
                }
            }

            enum [<$name:camel "Update">] {
                Observe(f64),
            }

            impl [<$name:camel>] {
                pub fn observe(data: impl Into<f64>) {
                    $crate::update([<$name:camel "Update">]::Observe(data.into()));
                }

                pub fn register() {
                    $crate::register_default_metric!(Self);
                }
            }

            impl $crate::MetricUpdate for [<$name:camel "Update">] {}

            impl $crate::Metric for [<$name:camel>] {
                fn poll(&self) -> Vec<$crate::Measurement> {
                    let sample_count = self.histogram.get_sample_count() as f64;
                    let average = if sample_count == 0.0 {None} else {Some(self.histogram.get_sample_sum() / sample_count)};
                    vec![$crate::Measurement::new(
                        &self.measurement_title,
                        average,
                        $label,
                        "",
                    )]
                }

                fn update(&mut self, update: Box<dyn std::any::Any>) {
                    if let Ok(mhu) = update.downcast::<[<$name:camel "Update">]>() {
                        match *mhu {
                            [<$name:camel "Update">]::Observe(d) => self.histogram.observe(d),
                        }
                    }
                }

                fn get_update_type_id(&self) -> std::any::TypeId {
                    std::any::TypeId::of::<[<$name:camel "Update">]>()
                }
            }
        }
    };

    ($name:ty, $label:literal) => {
        paste::paste! {
            $crate::create_histogram_metric!(
                $name,
                $label,
                prometheus::HistogramOpts::new(
                    $crate::Measurement::new(stringify!([<$name:snake>] hist), None, "", "").title_as_namespaced(),
                    stringify!([<$name:camel "help">]))
            );
        }
    };

    ($name:ty) => {
        paste::paste! {
            $crate::create_histogram_metric!(
                $name,
                "",
                prometheus::HistogramOpts::new(
                    $crate::Measurement::new(stringify!([<$name:snake>] hist), None, "", "").title_as_namespaced(),
                    stringify!([<$name:camel "help">]))
            );
        }
    };
}

///
/// Create a [`Metric`](crate::Metric) which wraps a [`prometheus::Histogram`](prometheus::Histogram).
///
/// Similar to [`create_histogram_metric`](crate::create_histogram_metric) but allows you to more
/// easily specify the buckets. The Prometheus bucket macros, such as
/// [`linear_buckets`](prometheus::linear_buckets) can be used here as well.
///
#[macro_export]
macro_rules! create_histogram_metric_with_buckets {
    ($name:ty, $label:literal, $buckets: expr) => {
        paste::paste! {
            $crate::create_histogram_metric!(
                $name,
                $label,
                prometheus::HistogramOpts::new(
                    $crate::Measurement::new(stringify!([<$name:snake>] hist), None, "", "").title_as_namespaced(),
                    stringify!([<$name:camel "help">]))
                    .buckets($buckets)
            );
        }
    };
    ($name:ty, $buckets: expr) => {
        paste::paste! {
            $crate::create_histogram_metric!(
                $name,
                "",
                prometheus::HistogramOpts::new(
                    $crate::Measurement::new(stringify!([<$name:snake>] hist), None, "", "").title_as_namespaced(),
                    stringify!([<$name:camel "help">]))
                    .buckets($buckets)
            );
        }
    };
}

///
/// Register the Default version of a Metric [`Metric`](crate::Metric) for tracking.
///
/// This should be called with the name passed to the creation macro and after
/// [`init()`](crate::MetricsInitializer::init) has been called. Macro-created Metrics should call
/// the `register()` associated function instead of using this macro.
///
#[macro_export]
macro_rules! register_default_metric {
    ($name:ty) => {
        paste::paste! {
            $crate::register(|| Box::<[<$name:camel>]>::default());
        }
    };
}

#[cfg(test)]
mod test {
    use crate::MetricsInitializer;
    use std::sync::Once;

    //
    // Generate test Metrics
    //
    create_metric!(TestMetric1);
    create_metric!(TestMetric2, "test label");
    create_metric!(UnregisteredMetric);

    //
    // Generate test Histogram Metrics
    //
    create_histogram_metric!(TestHistogram1);
    create_histogram_metric!(TestHistogram2, "histogram_label");
    create_histogram_metric!(
        TestHistogram3,
        "label",
        prometheus::histogram_opts!("My_Custom_Name", "help")
    );

    create_histogram_metric_with_buckets!(TestHistogram4, vec![25.0, 50.0, 75.0, 100.0]);
    create_histogram_metric_with_buckets!(
        TestHistogram5,
        "label",
        prometheus::linear_buckets(0.0, 10.0, 10).unwrap()
    );

    //
    // Generate Stats Metrics
    //
    create_stats_metric!(StatsMetric1);
    create_stats_metric!(StatsMetric2);

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            // No exporters used during these tests - including Pushgateway
            MetricsInitializer::default()
                .init()
                .expect("Failed to init Metrics");
        });
    }

    #[test]
    fn histogram_macros() {
        setup();

        TestHistogram1::register();
        TestHistogram2::register();
        TestHistogram3::register();
        TestHistogram4::register();
        TestHistogram5::register();

        TestHistogram1::observe(1.0);
        TestHistogram2::observe(2);
        TestHistogram3::observe(3.7);
        TestHistogram4::observe(472.5);
        TestHistogram5::observe(42);
    }

    #[test]
    fn metric_macros() {
        setup();

        TestMetric1::register();
        TestMetric2::register();

        TestMetric1::inc();
        TestMetric1::dec();
        TestMetric1::inc_by(41);
        TestMetric1::dec_by(5);

        TestMetric2::update(4567.89);

        // Unregistered metrics will not panic but should log a warning
        UnregisteredMetric::update(472.5);
    }

    #[test]
    fn stats_metric_macros() {
        setup();

        StatsMetric1::register();
        StatsMetric2::register_sticky();

        let data = 1.0;

        StatsMetric1::update_with(move || (data + 5.0 - 3.0) / 3.0);
        StatsMetric1::update_last(move |last| last + 5.0);
        StatsMetric1::update(7.0);
        StatsMetric1::inc();
        StatsMetric1::dec();
        StatsMetric1::inc_by(2.0);
        StatsMetric1::dec_by(2.0);

        let data = 2.0;

        StatsMetric2::update_with(move || (data + 5.0 - 3.0) / 2.0);
        StatsMetric2::update_last(move |last| last + 5.0);
        StatsMetric2::update(7.0);
        StatsMetric2::inc();
        StatsMetric2::dec();
        StatsMetric2::inc_by(2.0);
        StatsMetric2::dec_by(2.0);
    }

    #[test]
    fn use_prometheus() {
        setup();

        let opts = prometheus::histogram_opts!("test_prometheus_histogram", "help");
        let r = prometheus::register_histogram!(opts);
        assert!(r.is_ok());

        let ph = r.unwrap();
        ph.observe(4.0);
    }
}
