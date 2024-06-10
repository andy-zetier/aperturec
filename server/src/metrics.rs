use aperturec_metrics::exporters::{CsvExporter, Exporter, LogExporter, PushgatewayExporter};
use aperturec_metrics::{
    create_histogram_metric_with_buckets, create_metric, create_stats_metric, Measurement, Metric,
    MetricUpdate, MetricsInitializer,
};
use aperturec_trace::{log, Level};

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

///
/// Round Trip Time - Measures the time it takes a single message to be sent from the Server,
/// received by the Client, sent back by the Client, and ultimately received by the Server.
///
#[derive(Debug, Default, PartialEq)]
pub struct Rtt {
    in_flight: BTreeMap<u64, Instant>,
    rtts: Vec<Duration>,
}

pub enum RttUpdate {
    Outgoing(u64, Instant),
    Incoming(u64, Instant),
}

impl MetricUpdate for RttUpdate {}

impl Metric for Rtt {
    fn poll(&self) -> Vec<Measurement> {
        // millisecond scale
        let scale = 1000_f64;

        let mut period_avg = None;
        let mut period_max = None;
        let mut period_var = None;

        if !self.rtts.is_empty() {
            let sum = self.rtts.iter().sum::<Duration>().as_secs_f64();
            let count = self.rtts.len() as f64;
            let avg = sum / count;
            let max = self.rtts.iter().max().unwrap().as_secs_f64();
            let var = self
                .rtts
                .iter()
                .map(|r| {
                    let diff = avg - (r.as_secs_f64());
                    diff * diff
                })
                .sum::<f64>()
                / count;

            period_avg = Some(avg * scale);
            period_max = Some(max * scale);
            period_var = Some(var * scale);
        }

        vec![
            Measurement::new("RTT Avg", period_avg, "ms", "Average Round Trip Time"),
            Measurement::new("RTT Max", period_max, "ms", "Max Round Trip Time"),
            Measurement::new("RTT Var", period_var, "ms", "Round Trip Time Variance"),
        ]
    }

    fn reset(&mut self) {
        self.in_flight.retain(|_, v| v.elapsed().as_secs() <= 60);
        self.rtts.clear();
    }

    fn update(&mut self, update: Box<dyn std::any::Any>) {
        if let Ok(m) = update.downcast::<RttUpdate>() {
            match *m {
                RttUpdate::Outgoing(id, now) => {
                    self.in_flight.insert(id, now);
                }
                RttUpdate::Incoming(id, now) => {
                    let sent = match self.in_flight.remove(&id) {
                        Some(t) => t,
                        None => return,
                    };
                    self.rtts.push(now - sent)
                }
            }
        }
    }

    fn get_update_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<RttUpdate>()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn round_trip_time() {
        // milisecond scale
        let scale = 1000_f64;

        let mut rtt = Rtt::default();
        let v = rtt.poll();

        // RTT yields 3 Measurements, all initialize to None
        assert_eq!(v.len(), 3);
        v.iter().for_each(|m| assert_eq!(m.value, None));

        let time0 = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let time1 = Instant::now();
        let diff = (time1 - time0).as_secs_f64();

        rtt.update(RttUpdate::Outgoing(1, time0).to_boxed_any());
        rtt.update(RttUpdate::Incoming(1, time1).to_boxed_any());
        let v = rtt.poll();

        // RTT average for packet 1 is difference between time0 and time1
        assert_eq!(v[0].value.unwrap(), diff * scale);

        let time0 = Instant::now();
        std::thread::sleep(Duration::from_secs(1));
        let time1 = Instant::now();

        rtt.update(RttUpdate::Outgoing(2, time0).to_boxed_any());
        rtt.update(RttUpdate::Incoming(3, Instant::now()).to_boxed_any());
        let v = rtt.poll();

        // Rtt isn't affected by unmatched outgoing or incoming packets
        assert_eq!(v[0].value.unwrap(), diff * scale);
        assert_eq!(v[1].value.unwrap(), diff * scale);

        rtt.update(RttUpdate::Incoming(2, time1).to_boxed_any());
        let diff2 = (time1 - time0).as_secs_f64();
        let v = rtt.poll();

        // Average RTT is average of all measurements during the polling period
        assert_eq!(
            v[0].value.unwrap().round(),
            (((diff + diff2) / 2.0) * scale).round()
        );

        // Max RTT is equal to the first diff
        assert_eq!(v[1].value.unwrap().round(), (diff * scale).round());

        // Variance is about 250ms
        assert_eq!(v[2].value.unwrap().round(), 250.0);

        rtt.reset();

        // reset() clears Rtt
        assert_eq!(rtt, Rtt::default());
    }
}

pub fn rtt_outgoing(id: u64) {
    aperturec_metrics::update(RttUpdate::Outgoing(id, Instant::now()));
}

pub fn rtt_incoming(id: u64) {
    aperturec_metrics::update(RttUpdate::Incoming(id, Instant::now()));
}

create_histogram_metric_with_buckets!(
    CompressionRatio,
    "%",
    prometheus::linear_buckets(11.0, 11.0, 9).unwrap()
);

create_metric!(PixelsCompressed);
create_metric!(TimeInCompression);
create_metric!(FramebufferUpdatesSent);

create_histogram_metric_with_buckets!(
    TrackingBufferDamageRatio,
    "%",
    prometheus::linear_buckets(10., 10., 10).unwrap()
);

create_stats_metric!(WindowFillPercent, "%", 100.0);

pub fn setup_server_metrics(
    metrics_log: bool,
    metrics_csv: Option<String>,
    metrics_prom: Option<String>,
    prom_id: u32,
) {
    if metrics_prom.is_none() {
        log::warn!("Prometheus Pushgateway URL is not set. Use --metrics-prom to resolve");
    }

    if metrics_log || metrics_csv.is_some() || metrics_prom.is_some() {
        let mut exporters: Vec<Exporter> = vec![];
        if metrics_log {
            match LogExporter::new(Level::DEBUG) {
                Ok(le) => exporters.push(Exporter::Log(le)),
                Err(err) => log::warn!("Failed to setup Log exporter: {}, disabling", err),
            }
        }
        if let Some(path) = metrics_csv {
            match CsvExporter::new(path) {
                Ok(csve) => exporters.push(Exporter::Csv(csve)),
                Err(err) => log::warn!("Failed to setup CSV exporter: {}, disabling", err),
            }
        }
        if let Some(url) = metrics_prom {
            match PushgatewayExporter::new(url.to_owned(), "aperturec_server".to_owned(), prom_id) {
                Ok(pge) => exporters.push(Exporter::Pushgateway(pge)),
                Err(err) => log::warn!("Failed to setup Pushgateway exporter: {}, disabling", err),
            }
        }

        MetricsInitializer::default()
            .with_poll_rate_from_secs(3)
            .with_exporters(exporters)
            .init()
            .expect("Failed to setup metrics");

        ctrlc::set_handler(move || {
            aperturec_metrics::stop();
            std::process::exit(0);
        })
        .unwrap();

        aperturec_metrics::register_default_metric!(Rtt);

        CompressionRatio::register();
        TrackingBufferDamageRatio::register();
        PixelsCompressed::register();
        TimeInCompression::register();
        FramebufferUpdatesSent::register();
        WindowFillPercent::register();
    }
}
