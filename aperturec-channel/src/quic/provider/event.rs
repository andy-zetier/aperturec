use aperturec_metrics::create_metric;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use s2n_quic::provider::event::{
    self,
    events::{self, Frame},
};

fn application_bytes_in_frame(frame: &Frame) -> Option<usize> {
    match frame {
        Frame::Stream { len, .. } | Frame::Datagram { len, .. } => Some(*len as usize),
        _ => None,
    }
}

create_metric!(Mtu);
create_metric!(RttAverage);
create_metric!(RttVariance);
create_metric!(RttMax);
pub struct MetricsSubscriber;
pub struct MetricsSubscriberContext {
    rtt_stats: RollingStats,
}

impl MetricsSubscriberContext {
    const RTT_RETENTION_TIME: Duration = Duration::from_secs(1);
}

/// Rolling statistics over a time window for RTT samples.
///
/// `push` is amortized O(1) using deque eviction and a monotonic max queue,
/// while `stats` is O(n) and computes mean/variance with Welford's method.
struct RollingStats {
    /// Time window for retaining samples.
    window: Duration,
    /// Samples in monotonic time order.
    samples: VecDeque<Sample>,
    /// Monotonic deque of candidate maxima for O(1) max lookup.
    max_queue: VecDeque<Sample>,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    value: f64,
}

impl RollingStats {
    /// Creates a rolling statistics window that retains samples for `window`.
    fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
            max_queue: VecDeque::new(),
        }
    }

    /// Adds a new sample at `now`, evicting expired entries and updating the max queue.
    ///
    /// `now` must be monotonically non-decreasing across calls so that eviction uses
    /// `Instant::duration_since` safely.
    ///
    /// # Panics
    ///
    /// Panics if `now` is earlier than any stored sample timestamp.
    fn push(&mut self, now: Instant, value: f64) {
        if let Some(last) = self.samples.back() {
            debug_assert!(
                now >= last.at,
                "rolling stats sample timestamp moved backwards"
            );
        }
        while let Some(front) = self.samples.front().copied() {
            if now.duration_since(front.at) < self.window {
                break;
            }
            self.samples.pop_front();
            while let Some(max_front) = self.max_queue.front() {
                if max_front.at == front.at {
                    self.max_queue.pop_front();
                } else {
                    break;
                }
            }
        }

        let sample = Sample { at: now, value };
        self.samples.push_back(sample);

        while let Some(back) = self.max_queue.back() {
            if back.value > value {
                break;
            }
            self.max_queue.pop_back();
        }
        self.max_queue.push_back(sample);
    }

    /// Computes the mean, population variance, and max for the current window.
    ///
    /// Returns `None` when no samples are present.
    ///
    /// # Panics
    ///
    /// Panics if internal invariants are violated (non-empty samples with empty max queue).
    fn stats(&self) -> Option<(f64, f64, f64)> {
        if self.samples.is_empty() {
            return None;
        }
        let mut mean = 0.0;
        let mut m2 = 0.0;
        for (idx, sample) in self.samples.iter().enumerate() {
            let count = idx + 1;
            let delta = sample.value - mean;
            mean += delta / count as f64;
            let delta2 = sample.value - mean;
            m2 += delta * delta2;
        }
        let n = self.samples.len() as f64;
        let variance = (m2 / n).max(0.0);
        let max = self
            .max_queue
            .front()
            .copied()
            .map(|sample| sample.value)
            .expect("max queue empty with non-empty samples");
        Some((mean, variance, max))
    }
}

impl event::Subscriber for MetricsSubscriber {
    type ConnectionContext = MetricsSubscriberContext;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        Mtu::register();
        RttAverage::register();
        RttVariance::register();
        RttMax::register();

        MetricsSubscriberContext {
            rtt_stats: RollingStats::new(Self::ConnectionContext::RTT_RETENTION_TIME),
        }
    }

    fn on_mtu_updated(
        &mut self,
        _: &mut Self::ConnectionContext,
        _: &event::ConnectionMeta,
        event: &event::events::MtuUpdated,
    ) {
        Mtu::update(event.mtu);
    }

    fn on_recovery_metrics(
        &mut self,
        ctx: &mut Self::ConnectionContext,
        _: &event::ConnectionMeta,
        event: &event::events::RecoveryMetrics,
    ) {
        let now = Instant::now();

        ctx.rtt_stats.push(now, event.latest_rtt.as_secs_f64());
        if let Some((mean, variance, max)) = ctx.rtt_stats.stats() {
            RttAverage::update(mean);
            RttVariance::update(variance);
            RttMax::update(max);
        }
    }

    fn on_frame_sent(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameSent,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::tx_bytes(nbytes);
        }
    }

    fn on_frame_received(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameReceived,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::rx_bytes(nbytes);
        }
    }

    fn on_packet_dropped(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketDropped,
    ) {
        aperturec_metrics::builtins::packet_lost(1);
    }

    fn on_packet_lost(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketLost,
    ) {
        aperturec_metrics::builtins::packet_lost(1);
    }

    fn on_packet_sent(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketSent,
    ) {
        aperturec_metrics::builtins::packet_sent(1);
    }

    fn on_connection_closed(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::ConnectionClosed,
    ) {
        Mtu::clear();
        RttAverage::clear();
        RttVariance::clear();
        RttMax::clear();
    }
}

#[cfg(test)]
mod tests {
    use super::RollingStats;
    use std::time::{Duration, Instant};

    fn assert_close(actual: f64, expected: f64) {
        let tol = 1e-9;
        assert!(
            (actual - expected).abs() <= tol,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn rolling_stats_empty() {
        let stats = RollingStats::new(Duration::from_secs(1));
        assert!(stats.stats().is_none());
    }

    #[test]
    fn rolling_stats_mean_variance_max() {
        let mut stats = RollingStats::new(Duration::from_secs(1));
        let now = Instant::now();
        stats.push(now, 1.0);
        stats.push(now + Duration::from_millis(10), 3.0);
        stats.push(now + Duration::from_millis(20), 5.0);

        let (mean, variance, max) = stats.stats().expect("stats");
        assert_close(mean, 3.0);
        assert_close(variance, 8.0 / 3.0);
        assert_close(max, 5.0);
    }

    #[test]
    fn rolling_stats_expires_old_samples() {
        let mut stats = RollingStats::new(Duration::from_secs(1));
        let now = Instant::now();
        stats.push(now - Duration::from_millis(1500), 1.0);
        stats.push(now - Duration::from_millis(500), 2.0);
        stats.push(now, 3.0);

        let (mean, variance, max) = stats.stats().expect("stats");
        assert_close(mean, 2.5);
        assert_close(variance, 0.25);
        assert_close(max, 3.0);
    }

    #[test]
    fn rolling_stats_max_updates_after_expiry() {
        let mut stats = RollingStats::new(Duration::from_secs(1));
        let base = Instant::now();
        stats.push(base - Duration::from_millis(900), 10.0);
        stats.push(base - Duration::from_millis(100), 1.0);
        stats.push(base, 2.0);

        let (_, _, max) = stats.stats().expect("stats");
        assert_close(max, 10.0);

        stats.push(base + Duration::from_millis(200), 3.0);
        let (_, _, max) = stats.stats().expect("stats");
        assert_close(max, 3.0);
    }
}
