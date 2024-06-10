use aperturec_metrics::{register_default_metric, Measurement, Metric, MetricUpdate};

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Default, PartialEq)]
pub struct Timer {
    last_start: Option<Instant>,
    elapsed: Duration,
}

impl Timer {
    pub fn start(&mut self) {
        self.start_at(Instant::now())
    }

    pub fn start_at(&mut self, now: Instant) {
        if self.last_start.is_none() {
            self.last_start = Some(now);
        }
    }

    pub fn stop(&mut self) -> Duration {
        self.stop_at(Instant::now())
    }

    pub fn stop_at(&mut self, now: Instant) -> Duration {
        if let Some(time) = self.last_start {
            self.elapsed = self
                .elapsed
                .saturating_add(now.saturating_duration_since(time));
            self.last_start = None;
        }
        self.elapsed
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed_at(&Instant::now())
    }

    pub fn elapsed_at(&self, mark: &Instant) -> Duration {
        match self.last_start {
            Some(time) => self
                .elapsed
                .saturating_add(time.elapsed().saturating_sub(mark.elapsed())),
            None => self.elapsed,
        }
    }

    pub fn reset(&mut self) {
        if self.last_start.is_some() {
            self.last_start = Some(Instant::now());
        }
        self.elapsed = Duration::new(0, 0);
    }
}

#[derive(Debug, Default, PartialEq)]
struct Idle {
    idle_timers: BTreeMap<u32, Timer>,
    poll_timer: Timer,
}

enum IdleUpdate {
    Working(u32, Instant),
    Idle(u32, Instant),
}

impl MetricUpdate for IdleUpdate {}

impl Metric for Idle {
    fn poll(&self) -> Vec<Measurement> {
        let mark = Instant::now();
        let poll_duration = self.poll_timer.elapsed_at(&mark).as_secs_f64();
        let total_idle = self
            .idle_timers
            .values()
            .map(|t| t.elapsed_at(&mark).as_secs_f64())
            .sum();
        let total_poll = f64::max(total_idle, poll_duration * self.idle_timers.len() as f64);

        let idle_percent = (total_idle / total_poll) * 100.0;
        let idle_percent = if idle_percent.is_nan() {
            None
        } else {
            Some(idle_percent)
        };

        vec![Measurement::new(
            "Idle",
            idle_percent,
            "%",
            "Idle percentage",
        )]
    }

    fn reset(&mut self) {
        self.idle_timers.values_mut().for_each(|t| t.reset());
        self.poll_timer.reset();
    }

    fn update(&mut self, update: Box<dyn std::any::Any>) {
        if let Ok(m) = update.downcast::<IdleUpdate>() {
            self.poll_timer.start();
            match *m {
                IdleUpdate::Working(id, now) => self
                    .idle_timers
                    .entry(id)
                    .and_modify(|t| {
                        t.stop_at(now);
                    })
                    .or_default(),
                IdleUpdate::Idle(id, now) => self
                    .idle_timers
                    .entry(id)
                    .and_modify(|t| t.start_at(now))
                    .or_default(),
            };
        }
    }

    fn get_update_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<IdleUpdate>()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn idle_timer() {
        let mut timer = Timer::default();

        // Timer defaults to not started
        assert!(timer.last_start.is_none());

        let time0 = Instant::now();
        timer.start_at(time0);
        let time1 = timer.stop();

        // Timer has required precision
        assert!(time1.as_secs_f64() > 0.0);

        let sleep_duration = Duration::from_millis(1234);
        timer.start();
        std::thread::sleep(sleep_duration);
        let stop_duration = timer.stop();
        let stop_secs_f64 = (stop_duration.as_secs_f64() * 1000.0).round() / 1000.0;

        // Timer is (roughly (3 decimal places)) equal to the sleep time
        assert_eq!(sleep_duration.as_secs_f64(), stop_secs_f64);

        // A stopped timer's elapsed equals stop_duration
        assert_eq!(timer.elapsed(), stop_duration);

        timer.start();
        std::thread::sleep(sleep_duration);
        let check_duration = timer.elapsed();
        let check_secs_f64 = (check_duration.as_secs_f64() * 1000.0).round() / 1000.0;

        // Timer.elapsed() returns the correct time
        assert_eq!(check_secs_f64, stop_secs_f64 + sleep_duration.as_secs_f64());

        timer.stop();
        timer.reset();

        // Timer.reset() clears the timer
        assert_eq!(Timer::default(), timer);

        timer.start();
        std::thread::sleep(sleep_duration);
        let mark = Instant::now();
        std::thread::sleep(sleep_duration);
        let elapsed =
            ((timer.elapsed() - timer.elapsed_at(&mark)).as_secs_f64() * 1000.0).round() / 1000.0;

        // Check elapsed_at correctly calculates the elapsed time up till mark
        assert_eq!(elapsed, sleep_duration.as_secs_f64());
        timer.start();
    }
}

aperturec_metrics::create_stats_metric!(WindowFillPercent, "%", 100.0);
aperturec_metrics::create_stats_metric!(WindowAdvanceLatency, "ms", 1000.0);

pub fn idling(id: u32) {
    aperturec_metrics::update(IdleUpdate::Idle(id, Instant::now()));
}

pub fn working(id: u32) {
    aperturec_metrics::update(IdleUpdate::Working(id, Instant::now()));
}

pub fn setup_client_metrics() {
    register_default_metric!(Idle);
    WindowAdvanceLatency::register_sticky();
    WindowFillPercent::register();
}
