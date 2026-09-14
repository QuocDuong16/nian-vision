use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use sysinfo::{Networks, Pid, ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerformanceSnapshotDto {
    pub sample_ready: bool,
    pub process_count: usize,
    pub app_cpu_percent: f32,
    pub system_cpu_percent: f32,
    pub app_memory_bytes: u64,
    pub system_memory_used_bytes: u64,
    pub system_memory_total_bytes: u64,
    pub system_network_rx_bps: u64,
    pub system_network_tx_bps: u64,
    pub app_network_rx_bps: Option<u64>,
    pub app_network_tx_bps: Option<u64>,
    pub app_gpu_percent: Option<f32>,
    pub system_gpu_percent: Option<f32>,
}

struct PerformanceSampler {
    system: System,
    networks: Networks,
    last_sample: Instant,
    samples: u64,
}

impl PerformanceSampler {
    fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        Self {
            system,
            networks: Networks::new_with_refreshed_list(),
            last_sample: Instant::now(),
            samples: 0,
        }
    }

    fn sample(&mut self) -> PerformanceSnapshotDto {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.networks.refresh(true);

        let elapsed = self.last_sample.elapsed().as_secs_f64().max(0.001);
        self.last_sample = Instant::now();
        self.samples = self.samples.saturating_add(1);

        let root_pid = Pid::from_u32(std::process::id());
        let process_tree = process_tree(&self.system, root_pid);
        let logical_cpu_count = self.system.cpus().len().max(1) as f32;
        let app_cpu_percent = process_tree
            .iter()
            .filter_map(|pid| self.system.process(*pid))
            .map(|process| process.cpu_usage())
            .sum::<f32>()
            / logical_cpu_count;
        let app_memory_bytes = process_tree
            .iter()
            .filter_map(|pid| self.system.process(*pid))
            .fold(0_u64, |total, process| {
                total.saturating_add(process.memory())
            });

        let (received, transmitted) =
            self.networks
                .iter()
                .fold((0_u64, 0_u64), |(received, transmitted), (_, data)| {
                    (
                        received.saturating_add(data.received()),
                        transmitted.saturating_add(data.transmitted()),
                    )
                });

        PerformanceSnapshotDto {
            sample_ready: self.samples > 1,
            process_count: process_tree.len(),
            app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
            system_cpu_percent: self.system.global_cpu_usage().clamp(0.0, 100.0),
            app_memory_bytes,
            system_memory_used_bytes: self.system.used_memory(),
            system_memory_total_bytes: self.system.total_memory(),
            system_network_rx_bps: (received as f64 / elapsed).round() as u64,
            system_network_tx_bps: (transmitted as f64 / elapsed).round() as u64,
            // sysinfo deliberately does not expose portable per-process network or GPU
            // accounting. Returning None is preferable to presenting disk/process I/O as
            // if it were network traffic, or vendor-specific GPU counters as universal.
            app_network_rx_bps: None,
            app_network_tx_bps: None,
            app_gpu_percent: None,
            system_gpu_percent: None,
        }
    }
}

fn process_tree(system: &System, root: Pid) -> HashSet<Pid> {
    let mut tree = HashSet::from([root]);
    let mut changed = true;
    while changed {
        changed = false;
        for (pid, process) in system.processes() {
            if tree.contains(pid) {
                continue;
            }
            if process
                .parent()
                .is_some_and(|parent| tree.contains(&parent))
            {
                tree.insert(*pid);
                changed = true;
            }
        }
    }
    tree
}

static PERFORMANCE_SAMPLER: OnceLock<Mutex<PerformanceSampler>> = OnceLock::new();

pub(crate) fn performance_snapshot() -> Result<PerformanceSnapshotDto, &'static str> {
    let sampler = PERFORMANCE_SAMPLER.get_or_init(|| Mutex::new(PerformanceSampler::new()));
    let mut sampler = sampler
        .lock()
        .map_err(|_| "performance sampler unavailable")?;
    Ok(sampler.sample())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_always_contains_current_process() {
        let system = System::new_all();
        let root = Pid::from_u32(std::process::id());
        assert!(process_tree(&system, root).contains(&root));
    }
}
