use crate::builtins;
use crate::exporters::Exporter;
use crate::{Metric, MetricUpdate, SysinfoMetric};

use anyhow::Result;
use std::any;
use std::collections::BTreeMap;
use std::sync::{mpsc, Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tracing::*;

static WARN_ONCE: Once = Once::new();

//
// Metrics updates are sent over a global mpsc to the metrics thread
//
static METRIC_COMS: OnceLock<Mutex<MetricComs>> = OnceLock::new();

#[derive(Debug)]
struct MetricComs {
    tx: Option<mpsc::Sender<MetricsMsg>>,
    stop_rx: Option<mpsc::Receiver<()>>,
}

//
// Messages to be sent to the metrics thread
//
enum MetricsMsg {
    Stop,
    Register(Box<dyn FnOnce() -> Box<dyn Metric> + Send + 'static>),
    Update(Box<dyn any::Any + Send + 'static>),
}

fn warn_once(msg: &str) {
    WARN_ONCE.call_once(|| warn!("{}", msg));
}

///
/// Register a new [`Metric`](crate::Metric) for tracking
///
pub fn register(create: impl FnOnce() -> Box<dyn Metric> + Send + 'static) {
    if let Some(mc) = METRIC_COMS.get() {
        if let Some(tx) = &mc.lock().unwrap().tx {
            match tx.send(MetricsMsg::Register(Box::new(create))) {
                Err(e) => warn!("Failed to register metric: {}", e),
                _ => return,
            }
        }
    }

    warn_once("Metrics not initialized");
}

///
/// Send a [`MetricUpdate`](crate::MetricUpdate) to a previously registered [`Metric`](crate::Metric)
///
pub fn update(update: impl MetricUpdate + Send) {
    if let Some(mc) = METRIC_COMS.get() {
        if let Some(tx) = &mc.lock().unwrap().tx {
            match tx.send(MetricsMsg::Update(update.to_boxed_any())) {
                Err(e) => warn!("Failed to update metric: {}", e),
                _ => return,
            }
        }
    }

    warn_once("Metrics not initialized");
}

///
/// Stop metric tracking
///
/// This function should be called before the process terminates to ensure each
/// [`Exporter`](crate::exporters::Exporter) is shut down properly.
///
pub fn stop() {
    if let Some(mc) = METRIC_COMS.get() {
        if let Ok(mut mg) = mc.lock() {
            if let Some(tx) = mg.tx.take() {
                match tx.send(MetricsMsg::Stop) {
                    Err(e) => warn!("Failed to stop metrics: {}", e),
                    Ok(_) => match mg.stop_rx.take() {
                        Some(rx) => {
                            let _ = rx.recv();
                            return;
                        }
                        None => panic!("stop_rx is None"),
                    },
                }
            }
        }
    }

    warn_once("Metrics not initialized");
}

///
/// Used to configure and launch metrics tracking
///
/// This struct's [`init()`](MetricsInitializer::init) must be called before
/// [`register()`](register), [`update()`](stop), or [`stop()`] can be called.
///
/// Calling [`init()`](MetricsInitializer::init) will launch a new metrics thread which is responsible for registering new
/// [`Metric`]s, listening for and dispatching [`update()`]s to the appropriate [`Metric`]s, and
/// passing [`Meausurement`](crate::Measurement)s to each [`Exporter`](crate::exporters::Exporter) at the polling
/// rate.
///
/// Example:
/// ```
/// # use tracing::Level;
/// use aperturec_metrics::MetricsInitializer;
/// use aperturec_metrics::builtins;
/// use aperturec_metrics::exporters::{Exporter, LogExporter};
///
/// MetricsInitializer::default()
///     .with_poll_rate_from_secs(3)
///     .with_exporter(Exporter::Log(LogExporter::new(Level::DEBUG).unwrap()))
///     .with_builtin(builtins::BuiltinMetric::PacketLoss)
///     .init()
///     .expect("Failed to init metrics");
///
/// // Update packet loss metric
/// aperturec_metrics::builtins::packet_sent(5);
/// aperturec_metrics::builtins::packet_lost(1);
///
/// // Stop metric tracking
/// aperturec_metrics::stop();
/// ```
pub struct MetricsInitializer {
    exporters: Vec<Exporter>,
    builtins: Option<Vec<builtins::BuiltinMetric>>,
    poll_rate: Duration,
}

impl Default for MetricsInitializer {
    fn default() -> Self {
        MetricsInitializer {
            exporters: vec![],
            builtins: None,
            poll_rate: Self::min_rate(),
        }
    }
}

impl MetricsInitializer {
    fn min_rate() -> Duration {
        static MIN_REFRESH_RATE: OnceLock<Duration> = OnceLock::new();
        *MIN_REFRESH_RATE.get_or_init(|| {
            std::cmp::max(Duration::from_secs(3), sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)
        })
    }

    pub fn with_builtin(mut self, builtin: builtins::BuiltinMetric) -> Self {
        match self.builtins {
            Some(ref mut builtins) => builtins.push(builtin),
            None => self.builtins = Some(vec![builtin]),
        };
        self
    }

    pub fn with_builtins<T>(mut self, iter: T) -> Self
    where
        T: IntoIterator<Item = builtins::BuiltinMetric>,
    {
        match self.builtins {
            Some(ref mut builtins) => builtins.extend(iter),
            None => self.builtins = Some(Vec::from_iter(iter)),
        };
        self
    }

    pub fn with_exporter(mut self, exporter: Exporter) -> Self {
        self.exporters.push(exporter);
        self
    }

    pub fn with_exporters<T>(mut self, iter: T) -> Self
    where
        T: IntoIterator<Item = Exporter>,
    {
        self.exporters.extend(iter);
        self
    }

    pub fn with_poll_rate(mut self, poll_rate: Duration) -> Self {
        let mut new_rate = poll_rate;
        if new_rate < Self::min_rate() {
            warn!(
                "Polling rate {:?} is too low, using {:?}",
                poll_rate,
                Self::min_rate()
            );
            new_rate = Self::min_rate();
        }
        self.poll_rate = new_rate;
        self
    }

    pub fn with_poll_rate_from_secs(self, secs: u64) -> Self {
        self.with_poll_rate(Duration::from_secs(secs))
    }

    pub fn init(mut self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();

        METRIC_COMS
            .set(Mutex::new(MetricComs {
                tx: Some(tx),
                stop_rx: Some(stop_rx),
            }))
            .expect("Metrics mutex");

        let rate = self.poll_rate;

        // Use all builtins if none were specified
        let builtins = match self.builtins {
            Some(ref builtins) => builtins.clone(),
            None => vec![
                builtins::BuiltinMetric::PacketLoss,
                builtins::BuiltinMetric::TxRxRate,
                builtins::BuiltinMetric::CpuUsage,
                builtins::BuiltinMetric::MemoryUsage,
            ],
        };

        let thread_builtins = builtins.clone();

        //
        // MetricsInitializer spawns two threads: an outer and an inner. The outer thread monitors the
        // inner thread's join handle ensuring Drop handlers have a chance to run when the
        // inner thread terminates. The join handle is monitored by the outer thread and not
        // the MetricsInitializer struct itself to support the static interface for Metrics. This way,
        // stop() can be called statically and does not need to keep track of the join
        // handle.
        //

        let mut sys = System::new();
        let pid = sysinfo::get_current_pid().expect("current pid");
        let inner_jh = thread::spawn(move || {
            let mut metrics: BTreeMap<any::TypeId, Box<dyn Metric>> = BTreeMap::new();

            let sysinfo_metrics: Vec<Box<dyn SysinfoMetric>> = thread_builtins
                .iter()
                .filter_map(|sm| builtins::init_builtin_sysinfo_metric(sm, pid, &mut sys))
                .collect();

            // Build a sysinfo::RefreshKind from the Kinds published by all SysinfoMetrics
            let refresh_kind = sysinfo_metrics
                .iter()
                .fold(ProcessRefreshKind::nothing(), |acc, sm| {
                    sm.with_refresh_kind(acc)
                });
            let pids = [pid];
            let procs_to_update = ProcessesToUpdate::Some(&pids);

            sys.refresh_processes_specifics(procs_to_update, true, refresh_kind);

            sysinfo_metrics.iter().for_each(|sm| {
                let measurements = sm.poll_with_sys(&sys);
                let m_titles = measurements
                    .iter()
                    .map(|m| m.title.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                debug!("Registering builtin Metric measurements: {}", m_titles);
                self.exporters.iter_mut().for_each(|ex| {
                    if let Err(e) = ex.register_measurements(&measurements) {
                        error!(
                            "Failed to register measurements {:?} with exporter {:?}: {:?}",
                            m_titles, ex, e
                        );
                    }
                });
            });

            let mut timeout = rate;

            loop {
                let now = Instant::now();
                match rx.recv_timeout(timeout) {
                    Ok(mm) => {
                        match mm {
                            MetricsMsg::Stop => break,
                            MetricsMsg::Register(metric) => {
                                let metric = (metric)();
                                let tid = metric.get_update_type_id();
                                let measurements = metric.poll();
                                let m_titles = measurements
                                    .iter()
                                    .map(|m| m.title.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                match metrics.insert(tid, metric) {
                                    None => {
                                        debug!("Registering Metric measurements: {}", m_titles);
                                        trace!("Added Metric for update type {:?}", tid);
                                    }
                                    Some(_) => {
                                        warn!("Replaced existing Metric for update type {:?}", tid)
                                    }
                                }

                                self.exporters.iter_mut().for_each(|ex| {
                                    if let Err(e) = ex.register_measurements(&measurements) {
                                        error!(
                                            "Failed to register measurements {:?} with exporter {:?}: {:?}",
                                            m_titles, ex, e
                                        );
                                    }
                                });
                            }
                            MetricsMsg::Update(update) => {
                                let tid = (*update).type_id();
                                match metrics.get_mut(&tid) {
                                    None => {
                                        warn!("No Metric registered for update type {:?}", tid)
                                    }
                                    Some(ref mut m) => m.update(update),
                                }
                            }
                        }
                        let since_last_recv = std::cmp::max(Instant::now(), now) - now;
                        timeout = timeout - std::cmp::min(since_last_recv, timeout);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        //
                        // Polling rate expiry, poll metrics and send Measurements to Exporters
                        //

                        let mut measurements = metrics
                            .values_mut()
                            .map(|v| {
                                let r = v.poll();
                                v.reset();
                                r
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>();

                        sys.refresh_processes_specifics(procs_to_update, true, refresh_kind);

                        measurements.extend(
                            sysinfo_metrics
                                .iter()
                                .map(|sm| sm.poll_with_sys(&sys))
                                .collect::<Vec<_>>()
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>(),
                        );

                        self.exporters.iter_mut().for_each(|e| {
                            let _ = e.export(&measurements);
                        });

                        timeout = rate;
                    }
                    Err(err) => {
                        error!("{:?}", err);
                        break;
                    }
                }
            } // loop receive MetricsMsg

            debug!("Metrics thread exiting");
        }); // thread::spawn inner

        thread::spawn(move || {
            if let Err(err) = inner_jh.join() {
                warn!("Metrics thread failed to join: {:?}", err);
            }

            if let Err(err) = stop_tx.send(()) {
                warn!("Failed to send metrics stop notification: {}", err);
            }
        });

        //
        // Register built-in metrics
        //
        builtins.iter().for_each(builtins::register_builtin_metric);

        Ok(())
    }
}
