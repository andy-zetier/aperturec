pub mod builtins;
pub mod exporters;
mod measurement;
mod metrics;

pub mod macros;

pub use measurement::Measurement;
pub use metrics::{register, stop, update, MetricsInitializer};

///
/// Defines a metric to be tracked
///
/// Implementing this trait allows the metrics system to track additional metrics beyond those
/// included in [`builtins`].
///
pub trait Metric {
    fn poll(&self) -> Vec<Measurement> {
        vec![]
    }
    fn reset(&mut self) {}
    fn update(&mut self, update: Box<dyn std::any::Any>);
    fn get_update_type_id(&self) -> std::any::TypeId;
}

///
/// Defines an update to a tracked metric
///
/// Types that implement [`Metric`] can accept updates that implement this trait.
///
pub trait MetricUpdate: std::any::Any {
    fn to_boxed_any(self) -> Box<dyn std::any::Any + Send>
    where
        Self: Sized + Send,
    {
        Box::new(self) as Box<dyn std::any::Any + Send>
    }
}

trait SysinfoMetric {
    fn poll_with_sys(&self, sys: &sysinfo::System) -> Vec<Measurement>;
    fn with_refresh_kind(&self, kind: sysinfo::ProcessRefreshKind) -> sysinfo::ProcessRefreshKind;
}

#[allow(unused)]
trait IntrinsicMetric {
    fn poll(&self) -> Vec<Measurement>;
}
