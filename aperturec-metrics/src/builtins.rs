//!
//! Built-in metrics
//!
//! This module defines the built-in metrics included in this crate. These metrics are tracked by
//! default for any process that has initialized metrics tracking. They can be selectively enabled
//! via the [`with_builtin()`](crate::MetricsInitializer::with_builtin) method.
//!

use crate::IntrinsicMetric;
use crate::Measurement;
use crate::Metric;
use crate::MetricUpdate;
use crate::metrics::{register, update};

#[cfg(target_os = "linux")]
use procfs::{Current, CurrentSI};
use std::time::Instant;

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
    #[cfg(feature = "heap")]
    /// Measures various parts of heap usage
    Heap,
}

pub(crate) fn register_builtin_metric(metric: &BuiltinMetric) {
    match metric {
        BuiltinMetric::PacketLoss => register(|| Box::<PacketLoss>::default()),
        BuiltinMetric::TxRxRate => register(|| Box::<TxRxRate>::default()),
        _ => (), // Ignore Intrinsic metrics
    }
}

pub(crate) fn init_intrinsic_metric(metric: &BuiltinMetric) -> Option<Box<dyn IntrinsicMetric>> {
    match metric {
        #[cfg(target_os = "linux")]
        BuiltinMetric::CpuUsage => Some(Box::new(CpuUsage::default()) as Box<dyn IntrinsicMetric>),
        #[cfg(target_os = "linux")]
        BuiltinMetric::MemoryUsage => {
            Some(Box::new(MemoryUsage::default().with_scale(MemoryScale::Gb))
                as Box<dyn IntrinsicMetric>)
        }
        #[cfg(feature = "heap")]
        BuiltinMetric::Heap => Some(Box::new(Heap) as Box<dyn IntrinsicMetric>),
        _ => None,
    }
}

#[cfg(feature = "heap")]
struct Heap;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux;
#[cfg(not(target_os = "linux"))]
pub(crate) use non_linux::*;

#[cfg(feature = "heap")]
impl IntrinsicMetric for Heap {
    fn poll(&self) -> Vec<Measurement> {
        let info = malloc_info::malloc_info().expect("malloc_info");
        let mut system_max = 0;
        let mut system_curr = 0;
        for sys in info.system {
            match sys.r#type {
                malloc_info::info::SystemType::Current => system_curr += sys.size,
                malloc_info::info::SystemType::Max => system_max += sys.size,
                _ => (),
            }
        }
        let mut total_mmaps = 0;
        let mut total_mmap_size = 0;
        let mut total_fast_size = 0;
        let mut total_rest_size = 0;
        for total in info.total {
            match total.r#type {
                malloc_info::info::TotalType::Mmap => {
                    total_mmaps += total.count;
                    total_mmap_size += total.size;
                }
                malloc_info::info::TotalType::Fast => total_fast_size += total.size,
                malloc_info::info::TotalType::Rest => total_rest_size += total.size,
                _ => (),
            }
        }
        let total_free_size = total_fast_size + total_rest_size;
        vec![
            Measurement::new(
                "Heap System Max",
                Some(system_max as f64),
                "bytes",
                "Maximum amount of system memory ever used by the heap",
            ),
            Measurement::new(
                "Heap System Current",
                Some(system_curr as f64),
                "bytes",
                "Current amount of system memory in use by the heap",
            ),
            Measurement::new(
                "Heap Arena Count",
                Some(info.heaps.len() as f64),
                "",
                "Current number of arenas",
            ),
            Measurement::new(
                "Heap mmap Region Count",
                Some(total_mmaps as f64),
                "",
                "Total number of mmaped regions",
            ),
            Measurement::new(
                "Heap Total mmap Size",
                Some(total_mmap_size as f64),
                "bytes",
                "Total size of mmaped regions",
            ),
            Measurement::new(
                "Heap Total Free Size",
                Some(total_free_size as f64),
                "bytes",
                "Total size of free space in the heap",
            ),
            Measurement::new(
                "Heap Total fastbin Size",
                Some(total_fast_size as f64),
                "bytes",
                "Total size of free space in fastbins",
            ),
            Measurement::new(
                "Heap Total non fastbin Size",
                Some(total_rest_size as f64),
                "bytes",
                "Total size of free space in non-fastbins",
            ),
        ]
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

    pub(crate) fn find_primes(threads: usize, max: usize) {
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

    #[cfg(feature = "heap")]
    #[test]
    fn heap() {
        let measurements1 = Heap.poll();
        for i in 0..1024 {
            let mut v = vec![0_u8; i * 1024];
            v.iter_mut()
                .enumerate()
                .for_each(|(j, elem)| *elem = (j % u8::MAX as usize) as u8);
            if i % 2 == 0 {
                std::mem::forget(v);
            }
        }
        let measurements2 = Heap.poll();
        assert_eq!(measurements1.len(), measurements2.len());
        assert!(measurements1[0].value < measurements2[0].value);
        assert!(measurements1[1].value < measurements2[1].value);
    }
}
