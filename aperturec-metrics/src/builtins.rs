//!
//! Built-in metrics
//!
//! This module defines the built-in metrics included in this crate. These metrics are tracked by
//! default for any process that has initialized metrics tracking. They can be selectively enabled
//! via the [`with_builtin()`](crate::MetricsInitializer::with_builtin) method.
//!

use crate::metrics::{register, update};
use crate::Measurement;
use crate::Metric;
use crate::MetricUpdate;
use crate::SysinfoMetric;

use std::time::Instant;
use sysinfo::{CpuRefreshKind, Pid, ProcessExt, RefreshKind, System, SystemExt};

///
/// All available built-in metrics
///
/// These can be selected individually using the [`with_builtin()`](crate::MetricsInitializer::with_builtin) method.
///
#[derive(Clone, Debug)]
pub enum BuiltinMetric {
    /// Measures packet loss % within each polling period
    PacketLoss,
    /// Measures Transmit and Receive Rate within each polling period
    TxRxRate,
    /// Measures CPU % use by the process
    CpuUsage,
    /// Measures absolute `VIRT` and `RES` along with total % used
    MemoryUsage,
}

pub(crate) fn register_builtin_metric(metric: &BuiltinMetric) {
    match metric {
        BuiltinMetric::PacketLoss => register(|| Box::<PacketLoss>::default()),
        BuiltinMetric::TxRxRate => register(|| Box::<TxRxRate>::default()),
        _ => (), // Ignore SysinfoMetrics
    }
}

pub(crate) fn init_builtin_sysinfo_metric(
    si_metric: &BuiltinMetric,
) -> Option<Box<dyn SysinfoMetric>> {
    match si_metric {
        BuiltinMetric::CpuUsage => {
            Some(Box::new(CpuUsage::default().with_irix_mode(false)) as Box<dyn SysinfoMetric>)
        }
        BuiltinMetric::MemoryUsage => {
            Some(Box::new(MemoryUsage::default().with_scale(MemoryScale::Gb))
                as Box<dyn SysinfoMetric>)
        }
        _ => None, // Ignore Metrics
    }
}

//
// Packet Loss
//
#[derive(Debug, Default, PartialEq)]
struct PacketLoss {
    packets_sent: u64,
    packets_lost: u64,
}

enum PacketLossUpdate {
    Lost(usize),
    Sent(usize),
}

///
/// Measure the number of packets sent
///
/// Calling this function has no effect if [`PacketLoss`](BuiltinMetric::PacketLoss) has not been
/// selected for tracking during [`MetricsInitializer::init()`](crate::MetricsInitializer::init)
///
pub fn packet_sent(count: usize) {
    update(PacketLossUpdate::Sent(count));
}

///
/// Measure the number of packets received. This is an alias for
/// [`packet_sent()`](crate::builtins::packet_sent). Both measure the number of packets
/// transmitted.
///
/// Calling this function has no effect if [`PacketLoss`](BuiltinMetric::PacketLoss) has not been
/// selected for tracking during [`MetricsInitializer::init()`](crate::MetricsInitializer::init)
///
pub fn packet_received(count: usize) {
    // If the packet was successfully received, it must also have been sent
    update(PacketLossUpdate::Sent(count));
}

///
/// Measure the number of packets known to be lost
///
/// Calling this function has no effect if [`PacketLoss`](BuiltinMetric::PacketLoss) has not been
/// selected for tracking during [`MetricsInitializer::init()`](crate::MetricsInitializer::init)
///
pub fn packet_lost(count: usize) {
    update(PacketLossUpdate::Lost(count));
}

impl MetricUpdate for PacketLossUpdate {}

impl Metric for PacketLoss {
    fn poll(&self) -> Vec<Measurement> {
        let mut percent_loss = None;
        if self.packets_sent != 0 {
            let lost = self.packets_lost as f64;
            let sent = self.packets_sent as f64;
            percent_loss = Some((lost / sent) * 100.0);
        }

        vec![Measurement::new(
            "Packet Loss",
            percent_loss,
            "%",
            "Packet Loss Estimate",
        )]
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn update(&mut self, update: Box<dyn std::any::Any>) {
        if let Ok(plu) = update.downcast::<PacketLossUpdate>() {
            match *plu {
                PacketLossUpdate::Lost(l) => {
                    self.packets_lost = self.packets_lost.wrapping_add(l as u64)
                }
                PacketLossUpdate::Sent(s) => {
                    self.packets_sent = self.packets_sent.wrapping_add(s as u64)
                }
            }
        }
    }

    fn get_update_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<PacketLossUpdate>()
    }
}

//
// Transmit / Receive Rate
//
#[derive(Debug)]
struct TxRxRate {
    time_0: Instant,
    bytes_sent: u64,
    bytes_recv: u64,
}

enum TxRxRateUpdate {
    BytesSent(usize),
    BytesRecv(usize),
}

///
/// Measure the number of bytes transmitted
///
/// Calling this function has no effect if [`TxRxRate`](BuiltinMetric::TxRxRate) has not been
/// selected for tracking during [`MetricsInitializer::init()`](crate::MetricsInitializer::init)
///
pub fn tx_bytes(bytes_sent: usize) {
    update(TxRxRateUpdate::BytesSent(bytes_sent));
}

///
/// Measure the number of bytes received
///
/// Calling this function has no effect if [`TxRxRate`](BuiltinMetric::TxRxRate) has not been
/// selected for tracking during [`MetricsInitializer::init()`](crate::MetricsInitializer::init)
///
pub fn rx_bytes(bytes_recv: usize) {
    update(TxRxRateUpdate::BytesRecv(bytes_recv));
}

impl MetricUpdate for TxRxRateUpdate {}

impl Default for TxRxRate {
    fn default() -> Self {
        Self {
            time_0: Instant::now(),
            bytes_sent: 0,
            bytes_recv: 0,
        }
    }
}

impl PartialEq for TxRxRate {
    fn eq(&self, other: &Self) -> bool {
        self.bytes_sent == other.bytes_sent && self.bytes_recv == other.bytes_recv
    }
}

impl Metric for TxRxRate {
    fn poll(&self) -> Vec<Measurement> {
        let now = Instant::now();
        let (tx_mbps, rx_mbps) =
            if now > self.time_0 && (self.bytes_sent > 0 || self.bytes_recv > 0) {
                let secs = (now - self.time_0).as_secs_f64();
                let tx_mb = (self.bytes_sent as f64 * 8.0) / 1000.0 / 1000.0;
                let rx_mb = (self.bytes_recv as f64 * 8.0) / 1000.0 / 1000.0;
                (Some(tx_mb / secs), Some(rx_mb / secs))
            } else {
                (None, None)
            };

        vec![
            Measurement::new("Tx Rate", tx_mbps, "Mbps", "Transmit Rate"),
            Measurement::new("Rx Rate", rx_mbps, "Mbps", "Receive Rate"),
        ]
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn update(&mut self, update: Box<dyn std::any::Any>) {
        if let Ok(m) = update.downcast::<TxRxRateUpdate>() {
            match *m {
                TxRxRateUpdate::BytesSent(b) => {
                    self.bytes_sent = self.bytes_sent.wrapping_add(b as u64)
                }
                TxRxRateUpdate::BytesRecv(b) => {
                    self.bytes_recv = self.bytes_recv.wrapping_add(b as u64)
                }
            }
        }
    }

    fn get_update_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<TxRxRateUpdate>()
    }
}

//
// CPU Usage
//
struct CpuUsage {
    pid: Pid,
    irix_mode: bool,
}

impl CpuUsage {
    fn new() -> Self {
        let pid = match sysinfo::get_current_pid() {
            Ok(pid) => pid,
            Err(_) => panic!("Unsupported platform"),
        };
        Self {
            pid,
            irix_mode: false,
        }
    }

    pub fn with_irix_mode(mut self, is_on: bool) -> Self {
        self.irix_mode = is_on;
        self
    }
}

impl Default for CpuUsage {
    fn default() -> Self {
        Self::new()
    }
}

impl SysinfoMetric for CpuUsage {
    fn poll_with_sys(&self, sys: &System) -> Vec<Measurement> {
        let proc = sys
            .process(self.pid)
            .expect("Failed to get current process");
        let mut cpu_usage = proc.cpu_usage();

        if !self.irix_mode {
            cpu_usage /= sys.cpus().len() as f32;
        }

        vec![Measurement::new(
            "CPU Usage",
            Some(cpu_usage.into()),
            "%",
            "CPU Usage",
        )]
    }

    fn add_refresh_kind(&self, kind: &mut RefreshKind) {
        let _ = kind.with_cpu(CpuRefreshKind::new().with_cpu_usage());
    }
}

//
// Memory Usage
//
struct MemoryUsage {
    pid: Pid,
    installed_memory: u64,
    scale: MemoryScale,
}

#[allow(dead_code)]
enum MemoryScale {
    Bytes,
    Kb,
    Mb,
    Gb,
    Tb,
    Pb,
}

impl MemoryScale {
    fn bytes(&self, bytes: u64) -> f64 {
        let bytes = bytes as f64;
        match *self {
            MemoryScale::Bytes => bytes,
            MemoryScale::Kb => bytes / 1024.0_f64.powf(1.0),
            MemoryScale::Mb => bytes / 1024.0_f64.powf(2.0),
            MemoryScale::Gb => bytes / 1024.0_f64.powf(3.0),
            MemoryScale::Tb => bytes / 1024.0_f64.powf(4.0),
            MemoryScale::Pb => bytes / 1024.0_f64.powf(5.0),
        }
    }

    fn label(&self) -> &str {
        match self {
            MemoryScale::Bytes => "",
            MemoryScale::Kb => "KB",
            MemoryScale::Mb => "MB",
            MemoryScale::Gb => "GB",
            MemoryScale::Tb => "TB",
            MemoryScale::Pb => "PB",
        }
    }
}

impl MemoryUsage {
    fn new() -> Self {
        let pid = match sysinfo::get_current_pid() {
            Ok(pid) => pid,
            Err(_) => panic!("Unsupported platform"),
        };
        let sys = System::new_all();

        Self {
            pid,
            installed_memory: sys.total_memory(),
            scale: MemoryScale::Bytes,
        }
    }

    pub fn with_scale(mut self, scale: MemoryScale) -> Self {
        self.scale = scale;
        self
    }
}

impl Default for MemoryUsage {
    fn default() -> Self {
        Self::new()
    }
}

impl SysinfoMetric for MemoryUsage {
    fn poll_with_sys(&self, sys: &System) -> Vec<Measurement> {
        let proc = sys
            .process(self.pid)
            .expect("Failed to get current process");
        let mem = proc.memory();
        let vmem = proc.virtual_memory();

        vec![
            Measurement::new(
                "VMem Used",
                Some(self.scale.bytes(vmem)),
                self.scale.label(),
                "Virtual Memory Usage",
            ),
            Measurement::new(
                "Mem Used",
                Some(self.scale.bytes(mem)),
                self.scale.label(),
                "Resident Memory Size (RES)",
            ),
            Measurement::new(
                "Mem",
                Some((mem as f64 / self.installed_memory as f64) * 100.0),
                "%",
                "Resident Memory Size (RES) over total installed memory",
            ),
        ]
    }

    fn add_refresh_kind(&self, kind: &mut RefreshKind) {
        let _ = kind.with_memory();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use test_log::test;

    #[test]
    fn packet_loss() {
        let mut pl = PacketLoss::default();
        let v = pl.poll();

        // PacketLoss yields 1 Measurement and initializes to None
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].value, None);

        pl.update(PacketLossUpdate::Sent(25).to_boxed_any());
        pl.update(PacketLossUpdate::Lost(5).to_boxed_any());

        let v = pl.poll();

        // 5/25 = 20% Packet Loss
        assert_eq!(v[0].value.unwrap(), 20.0);

        pl.update(PacketLossUpdate::Lost(1).to_boxed_any());
        let v = pl.poll();

        // 5/26 = 24% Packet Loss
        assert_eq!(v[0].value.unwrap(), 24.0);

        pl.reset();

        // reset() clears PacketLoss
        assert_eq!(pl, PacketLoss::default());
    }

    #[test]
    fn tx_rx_rate() {
        let mut txrx = TxRxRate::default();
        let v = txrx.poll();

        // TxRx yields 2 Measurements, both initialize to None
        assert_eq!(v.len(), 2);
        v.iter().for_each(|m| assert_eq!(m.value, None));

        txrx.update(TxRxRateUpdate::BytesSent(99).to_boxed_any());
        txrx.update(TxRxRateUpdate::BytesRecv(42).to_boxed_any());

        let v = txrx.poll();
        std::thread::sleep(Duration::from_secs(1));
        let v2 = txrx.poll();

        // Data rates decrease as time goes on without updates
        v.iter().zip(v2).for_each(|(t0, t1)| {
            assert!(
                t1.value.unwrap() < t0.value.unwrap(),
                "{}: {} < {}",
                t1.title,
                t1.value.unwrap(),
                t0.value.unwrap()
            )
        });

        txrx.reset();

        // reset() clears TxRxRate
        assert_eq!(txrx, TxRxRate::default());
    }

    fn find_primes(threads: usize, max: usize) {
        let mut jhs = Vec::new();
        for _ in 0..threads {
            let jh = std::thread::spawn(move || {
                let mut primes = Vec::new();
                'outer: for num in 2..max {
                    if num <= 1 {
                        continue;
                    }
                    for i in 2..num {
                        if num % i == 0 {
                            continue 'outer;
                        }
                    }
                    primes.push(num);
                }
            });
            jhs.push(jh);
        }

        jhs.into_iter().for_each(|jh| jh.join().expect("join"));
    }

    #[test]
    fn cpu_usage() {
        let cu = CpuUsage::default();

        let mut refresh_kind = RefreshKind::new();
        cu.add_refresh_kind(&mut refresh_kind);
        let mut sys = System::new_with_specifics(refresh_kind);
        sys.refresh_all();

        let v = cu.poll_with_sys(&sys);

        // CpuUsage yields 1 Measurement and initializes to 0
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].value.unwrap(), 0.0);

        std::thread::sleep(System::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_all();
        let v = cu.poll_with_sys(&sys);
        let cpu0 = v[0].value.unwrap();

        std::thread::sleep(System::MINIMUM_CPU_UPDATE_INTERVAL);
        find_primes(sys.cpus().len(), 123456);
        sys.refresh_all();
        let v = cu.poll_with_sys(&sys);
        let cpu1 = v[0].value.unwrap();

        // CPU usage increases under heavy load
        assert!(cpu0 < cpu1, "{} < {}", cpu0, cpu1);
    }

    #[test]
    fn memory_usage() {
        let mu = MemoryUsage::default();

        let mut refresh_kind = RefreshKind::new();
        mu.add_refresh_kind(&mut refresh_kind);
        let mut sys = System::new_with_specifics(refresh_kind);
        sys.refresh_all();

        let v0 = mu.poll_with_sys(&sys);

        // MemoryUsage yields 3 Measurements
        assert_eq!(v0.len(), 3);

        let _lots_o_memory = vec![0; 1024 * 1024 * 500];

        let v1 = mu.poll_with_sys(&sys);

        // Memory usage increases with a large allocation
        v0.iter().zip(v1).for_each(|(m0, m1)| {
            assert!(
                m0.value.unwrap() <= m1.value.unwrap(),
                "{}: {} <= {}",
                m0.title,
                m0.value.unwrap(),
                m1.value.unwrap()
            )
        });
    }
}
