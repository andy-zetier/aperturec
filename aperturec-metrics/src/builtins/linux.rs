use super::*;

use std::cell::Cell;

//
// CPU Usage
//
pub(crate) struct CpuUsage {
    total_time: Cell<u64>,
    process_time: Cell<u64>,
}

impl Default for CpuUsage {
    fn default() -> Self {
        let kernel_cpu_total = procfs::KernelStats::current().expect("kernel stat").total;
        let mut total_time = kernel_cpu_total.user
            + kernel_cpu_total.nice
            + kernel_cpu_total.system
            + kernel_cpu_total.idle;
        if let Some(iowait) = kernel_cpu_total.iowait {
            total_time += iowait;
        }
        if let Some(irq) = kernel_cpu_total.irq {
            total_time += irq;
        }
        if let Some(steal) = kernel_cpu_total.steal {
            total_time += steal;
        }
        if let Some(guest) = kernel_cpu_total.guest {
            total_time += guest;
        }
        if let Some(nice) = kernel_cpu_total.guest_nice {
            total_time += nice;
        }
        let self_stat = procfs::process::Process::myself()
            .expect("myself")
            .stat()
            .expect("stat");
        let process_time = self_stat.utime + self_stat.stime;
        Self {
            total_time: Cell::new(total_time),
            process_time: Cell::new(process_time),
        }
    }
}

impl IntrinsicMetric for CpuUsage {
    fn poll(&self) -> Vec<Measurement> {
        let new = CpuUsage::default();
        let old_total = self.total_time.replace(new.total_time.get());
        let old_process = self.process_time.replace(new.process_time.get());

        let total_diff = new.total_time.get() - old_total;
        let process_diff = new.process_time.get() - old_process;
        let cpu_usage = {
            if total_diff == 0 {
                0.0
            } else {
                (process_diff as f64 / total_diff as f64) * 100.0
            }
        };

        vec![Measurement::new(
            "CPU Usage",
            Some(cpu_usage),
            "%",
            "CPU Usage",
        )]
    }
}

//
// Memory Usage
//
pub(crate) struct MemoryUsage {
    page_size: u64,
    installed_memory: u64,
    scale: MemoryScale,
}

impl MemoryUsage {
    pub(super) fn with_scale(mut self, scale: MemoryScale) -> Self {
        self.scale = scale;
        self
    }
}

impl Default for MemoryUsage {
    fn default() -> Self {
        let installed_memory = procfs::Meminfo::current().expect("meminfo").mem_total;
        Self {
            installed_memory,
            page_size: procfs::page_size(),
            scale: MemoryScale::Bytes,
        }
    }
}

impl IntrinsicMetric for MemoryUsage {
    fn poll(&self) -> Vec<Measurement> {
        let process = procfs::process::Process::myself().expect("myself");
        let statm = process.statm().expect("statm");
        let mem = statm.resident * self.page_size;
        let vmem = statm.size * self.page_size;

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
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::builtins::test::find_primes;

    #[test]
    fn cpu_usage() {
        let cu = CpuUsage::default();

        let v = cu.poll();

        // CpuUsage yields 1 Measurement
        assert_eq!(v.len(), 1);

        std::thread::sleep(crate::metrics::MetricsInitializer::min_rate());
        let v = cu.poll();
        let cpu0 = v[0].value.unwrap();

        std::thread::sleep(crate::metrics::MetricsInitializer::min_rate());
        find_primes(std::thread::available_parallelism().unwrap().into(), 123456);
        let v = cu.poll();
        let cpu1 = v[0].value.unwrap();

        // CPU usage increases under heavy load
        assert!(cpu0 < cpu1, "{cpu0} < {cpu1}");
    }

    #[test]
    fn memory_usage() {
        let mu = MemoryUsage::default();

        let v0 = mu.poll();

        // MemoryUsage yields 3 Measurements
        assert_eq!(v0.len(), 3);

        let _lots_o_memory = vec![0; 1024 * 1024 * 500];

        let v1 = mu.poll();

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
