///
/// Create a [`Metric`](crate::Metric).
///
/// This macro will generate the required structs and trait implementations to define a
/// [`Metric`](crate::Metric) that can be updated, incremented, or decremented. The
/// [`register_metric`](crate::register_metric) macro must be used to register this generated
/// [`Metric`](crate::Metric) for tracking. The same `$name` should be provided to
/// `create_metric()` and `register_metric()`.
///
/// The generated metric includes associated functions to modify the metric.
/// Presuming a metric name of `MyMetric`, the following functions are available:
///
/// * `MyMetric::update(new_value)` Sets the metric value to `new_value`
/// * `MyMetric::inc()` Increments the metric by 1
/// * `MyMetric::dec()` Decrements the metric by 1
/// * `MyMetric::inc_by(count)` Increments the metric by `count`
/// * `MyMetric::dec_by(count)` Decrements the metric by `count`
///
/// See [`register_metric`](crate::register_metric) for example usage.
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
/// Create a [`Metric`](crate::Metric) which wraps a [`prometheus::Histogram`](prometheus::Histogram).
///
/// This macro will generate the required structs and trait implementations to define a
/// [`Histogram`](prometheus::Histogram) managed by the metrics system. This allows you to call the
/// [`observe()`](prometheus::Histogram::observe) method without requiring a reference to the
/// underlying [`Histogram`](prometheus::Histogram). As with
/// [`create_metric`](crate::create_metric), you will need to call
/// [`register_metric`](crate::register_metric) to setup tracking.
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
/// See [`register_metric`](crate::register_metric) for example usage.
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
/// See [`register_metric`](crate::register_metric) for example usage.
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
/// Register a macro-created [`Metric`](crate::Metric) for tracking.
///
/// This should be called with the name passed to the creation macro and after
/// [`init()`](crate::MetricsInitializer::init) has been called.
///
/// Example:
///
/// ```no_run
/// # extern crate aperturec_quic_metrics as aperturec_metrics;
/// use aperturec_trace::Level;
/// use aperturec_metrics::MetricsInitializer;
/// use aperturec_metrics::exporters::{Exporter, LogExporter, PushgatewayExporter};
///
/// // Create a metric
/// aperturec_metrics::create_metric!(MyMetric, "label");
///
/// // Create a histogram metric
/// aperturec_metrics::create_histogram_metric!(MyHistogram);
///
/// fn main() {
///     MetricsInitializer::default()
///         .with_exporter(Exporter::Log(LogExporter::new(Level::DEBUG).unwrap()))
///         .with_exporter(Exporter::Pushgateway(PushgatewayExporter::new(
///             String::from("http://127.0.0.1:9091"),
///             String::from("my job"),
///             1234
///         ).unwrap()))
///         .init()
///         .expect("Failed to init metrics");
///
///     // Register the macro-created metrics
///     aperturec_metrics::register_metric!(MyMetric);
///     aperturec_metrics::register_metric!(MyHistogram);
///
///     // Increment the "MyMetric" metric
///     MyMetric::inc();
///
///     // Add an observation to "MyHistogram"
///     MyHistogram::observe(123.4);
///
///     aperturec_metrics::stop();
/// }
///```
///
#[macro_export]
macro_rules! register_metric {
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

        register_metric!(TestHistogram1);
        register_metric!(TestHistogram2);
        register_metric!(TestHistogram3);
        register_metric!(TestHistogram4);
        register_metric!(TestHistogram5);

        TestHistogram1::observe(1.0);
        TestHistogram2::observe(2);
        TestHistogram3::observe(3.7);
        TestHistogram4::observe(472.5);
        TestHistogram5::observe(42);
    }

    #[test]
    fn metric_macros() {
        setup();

        register_metric!(TestMetric1);
        register_metric!(TestMetric2);

        TestMetric1::inc();
        TestMetric1::dec();
        TestMetric1::inc_by(41);
        TestMetric1::dec_by(5);

        TestMetric2::update(4567.89);

        // Unregistered metrics will not panic but should log a warning
        UnregisteredMetric::update(472.5);
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
