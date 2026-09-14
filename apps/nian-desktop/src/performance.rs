use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use sysinfo::{Networks, Pid, ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerformanceProcessDto {
    pub pid: u32,
    pub name: String,
    pub role: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub is_root: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerformanceSnapshotDto {
    pub sample_ready: bool,
    pub process_count: usize,
    pub app_cpu_percent: f32,
    pub root_process_cpu_percent: f32,
    pub system_cpu_percent: f32,
    pub app_memory_bytes: u64,
    pub root_process_memory_bytes: u64,
    pub child_process_memory_bytes: u64,
    pub system_memory_used_bytes: u64,
    pub system_memory_total_bytes: u64,
    pub system_network_rx_bps: u64,
    pub system_network_tx_bps: u64,
    pub app_network_rx_bps: Option<u64>,
    pub app_network_tx_bps: Option<u64>,
    pub app_gpu_percent: Option<f32>,
    pub system_gpu_percent: Option<f32>,
    pub processes: Vec<PerformanceProcessDto>,
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
        let mut processes = process_tree
            .iter()
            .filter_map(|pid| {
                let process = self.system.process(*pid)?;
                let portable_memory = process.memory();
                let memory_bytes =
                    nian_platform_windows::process_private_working_set_bytes(pid.as_u32())
                        .unwrap_or(portable_memory);
                let cpu_percent = (process.cpu_usage() / logical_cpu_count).clamp(0.0, 100.0);
                Some(PerformanceProcessDto {
                    pid: pid.as_u32(),
                    name: process.name().to_string_lossy().into_owned(),
                    role: process_role(process, *pid == root_pid),
                    cpu_percent,
                    memory_bytes,
                    is_root: *pid == root_pid,
                })
            })
            .collect::<Vec<_>>();
        processes.sort_by(|left, right| {
            right
                .is_root
                .cmp(&left.is_root)
                .then_with(|| right.memory_bytes.cmp(&left.memory_bytes))
                .then_with(|| left.pid.cmp(&right.pid))
        });
        let root_process_cpu_percent = processes
            .iter()
            .find(|process| process.is_root)
            .map(|process| process.cpu_percent)
            .unwrap_or(0.0);
        let app_cpu_percent = processes
            .iter()
            .map(|process| process.cpu_percent)
            .sum::<f32>()
            .clamp(0.0, 100.0);
        let app_memory_bytes = processes.iter().fold(0_u64, |total, process| {
            total.saturating_add(process.memory_bytes)
        });
        let root_process_memory_bytes = processes
            .iter()
            .find(|process| process.is_root)
            .map(|process| process.memory_bytes)
            .unwrap_or(0);
        let child_process_memory_bytes = app_memory_bytes.saturating_sub(root_process_memory_bytes);

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
            process_count: processes.len(),
            app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
            root_process_cpu_percent: root_process_cpu_percent.clamp(0.0, 100.0),
            system_cpu_percent: self.system.global_cpu_usage().clamp(0.0, 100.0),
            app_memory_bytes,
            root_process_memory_bytes,
            child_process_memory_bytes,
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
            processes,
        }
    }
}

fn process_role(process: &sysinfo::Process, is_root: bool) -> String {
    let name = process.name().to_string_lossy();
    let command = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    process_role_from_parts(&name, &command, is_root).to_owned()
}

fn process_role_from_parts(name: &str, command: &str, is_root: bool) -> &'static str {
    let name = name.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    if is_root {
        return "Desktop host";
    }
    if name.contains("nian-media-worker") {
        return "Media worker";
    }
    if !name.contains("msedgewebview2") {
        return "Child process";
    }
    if command.contains("--type=gpu-process") {
        return "WebView2 GPU";
    }
    if command.contains("--type=renderer") {
        return "WebView2 renderer";
    }
    if command.contains("--type=crashpad-handler") {
        return "WebView2 crash handler";
    }
    if command.contains("--type=utility") {
        if command.contains("network.mojom.networkservice") {
            return "WebView2 network";
        }
        if command.contains("audio.mojom.audioservice") {
            return "WebView2 audio";
        }
        return "WebView2 utility";
    }
    "WebView2 browser"
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

    #[test]
    fn snapshot_breakdown_sums_to_reported_app_memory() {
        let mut sampler = PerformanceSampler::new();
        let snapshot = sampler.sample();
        assert!(!snapshot.processes.is_empty());
        assert_eq!(
            snapshot.processes.iter().fold(0_u64, |total, process| total
                .saturating_add(process.memory_bytes)),
            snapshot.app_memory_bytes
        );
        assert_eq!(
            snapshot
                .processes
                .iter()
                .filter(|process| process.is_root)
                .count(),
            1
        );
    }

    #[test]
    fn webview_process_roles_are_specific_enough_for_diagnostics() {
        assert_eq!(
            process_role_from_parts("msedgewebview2.exe", "--type=renderer", false),
            "WebView2 renderer"
        );
        assert_eq!(
            process_role_from_parts(
                "msedgewebview2.exe",
                "--type=utility --utility-sub-type=network.mojom.NetworkService",
                false,
            ),
            "WebView2 network"
        );
        assert_eq!(
            process_role_from_parts("msedgewebview2.exe", "", false),
            "WebView2 browser"
        );
        assert_eq!(
            process_role_from_parts("nian-media-worker.exe", "", false),
            "Media worker"
        );
    }
}
