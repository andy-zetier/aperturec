use super::*;

use sysinfo::{Pid, ProcessRefreshKind, System};

pub(crate) trait SysinfoMetric {
    fn poll_with_sys(&self, sys: &sysinfo::System) -> Vec<Measurement>;
    fn with_refresh_kind(&self, kind: sysinfo::ProcessRefreshKind) -> sysinfo::ProcessRefreshKind;
}

pub(crate) fn init_sysinfo_metric(
    si_metric: &BuiltinMetric,
    pid: Pid,
    sys: &mut System,
) -> Option<Box<dyn SysinfoMetric>> {
    match si_metric {
        BuiltinMetric::CpuUsage => {
            Some(Box::new(CpuUsage::new(pid).with_irix_mode(false)) as Box<dyn SysinfoMetric>)
        }
        BuiltinMetric::MemoryUsage => Some(Box::new(
            MemoryUsage::new(pid, sys).with_scale(MemoryScale::Gb),
        ) as Box<dyn SysinfoMetric>),
        _ => None, // Ignore Metrics
    }
}

//
// CPU Usage
//
pub(crate) struct CpuUsage {
    pid: Pid,
    irix_mode: bool,
}

impl CpuUsage {
    fn new(pid: Pid) -> Self {
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

    fn with_refresh_kind(&self, kind: ProcessRefreshKind) -> ProcessRefreshKind {
        kind.with_cpu()
    }
}

//
// Memory Usage
//
pub(crate) struct MemoryUsage {
    pid: Pid,
    installed_memory: u64,
    scale: MemoryScale,
}

impl MemoryUsage {
    fn new(pid: Pid, sys: &mut System) -> Self {
        sys.refresh_memory();
        Self {
            pid,
            installed_memory: sys.total_memory(),
            scale: MemoryScale::Bytes,
        }
    }

    fn with_scale(mut self, scale: MemoryScale) -> Self {
        self.scale = scale;
        self
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

    fn with_refresh_kind(&self, kind: ProcessRefreshKind) -> ProcessRefreshKind {
        kind.with_memory()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::builtins::test::find_primes;

    use sysinfo::ProcessesToUpdate;

    #[test]
    fn cpu_usage() {
        let pid = sysinfo::get_current_pid().expect("current pid");
        let refresh_kind = ProcessRefreshKind::nothing().with_cpu();
        let mut sys = System::new();

        let cu = CpuUsage::new(pid);
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind);

        let v = cu.poll_with_sys(&sys);

        // CpuUsage yields 1 Measurement and initializes to 0
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].value.unwrap(), 0.0);

        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_all();
        let v = cu.poll_with_sys(&sys);
        let cpu0 = v[0].value.unwrap();

        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        find_primes(sys.cpus().len(), 123456);
        sys.refresh_all();
        let v = cu.poll_with_sys(&sys);
        let cpu1 = v[0].value.unwrap();

        // CPU usage increases under heavy load
        assert!(cpu0 < cpu1, "{} < {}", cpu0, cpu1);
    }

    #[test]
    fn memory_usage() {
        let pid = sysinfo::get_current_pid().expect("current pid");
        let refresh_kind = ProcessRefreshKind::nothing().with_memory();
        let mut sys = System::new();

        let mu = MemoryUsage::new(pid, &mut sys);
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind);

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
