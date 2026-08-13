use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    time::Instant,
};

use crate::model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage};

#[derive(Debug, Clone, Copy)]
struct ProcessStat {
    parent_pid: u32,
    total_ticks: u64,
    start_ticks: u64,
    memory_bytes: u64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct PreviousProcess {
    total_ticks: u64,
    start_ticks: u64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
}

#[derive(Debug, Clone)]
struct ProcessUsage {
    parent_pid: u32,
    cpu_percent: f64,
    memory_bytes: u64,
    gpu_percent: f64,
    gpu_memory_bytes: u64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    open_files: HashMap<DiskFileId, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiskFileId {
    device: u64,
    inode: u64,
}

mod gpu;

use gpu::{GpuProcessStat, read_gpu_processes};

#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    processes: HashMap<u32, ProcessUsage>,
    children: HashMap<u32, Vec<u32>>,
    logical_cpus: usize,
    total_process_cpu_percent: f64,
    total_process_gpu_percent: f64,
    interval_seconds: f64,
    system_energy_mwh: f64,
    battery_full_mwh: f64,
    energy_source: String,
}

impl Default for ResourceSnapshot {
    fn default() -> Self {
        Self {
            processes: HashMap::new(),
            children: HashMap::new(),
            logical_cpus: 1,
            total_process_cpu_percent: 0.0,
            total_process_gpu_percent: 0.0,
            interval_seconds: 0.0,
            system_energy_mwh: 0.0,
            battery_full_mwh: 0.0,
            energy_source: "unavailable".into(),
        }
    }
}

impl ResourceSnapshot {
    pub fn usage_for_roots(&self, roots: impl IntoIterator<Item = u32>) -> ResourceUsage {
        let pids = descendants(roots, &self.children);
        let mut usage = ResourceUsage::default();
        let mut open_files = HashMap::new();
        for process in pids.iter().filter_map(|pid| self.processes.get(pid)) {
            usage.add_process(process);
            open_files.extend(
                process
                    .open_files
                    .iter()
                    .map(|(&file, &bytes)| (file, bytes)),
            );
        }
        usage.storage.open_file_disk_bytes = open_files.values().copied().sum();
        self.complete(usage)
    }

    fn complete(&self, mut usage: ResourceUsage) -> ResourceUsage {
        usage.compute.normalize_cpu(self.logical_cpus);
        usage.storage.normalize_rates(self.interval_seconds);
        let activity = usage.compute.cpu_percent + usage.compute.gpu_percent;
        let total = self.total_process_cpu_percent + self.total_process_gpu_percent;
        usage.energy = self.estimated_energy(activity, total);
        usage
    }

    fn estimated_energy(&self, activity: f64, total: f64) -> EnergyUsage {
        let share = if total > 0.0 {
            (activity / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let energy_mwh = rounded(self.system_energy_mwh * share, 4);
        let power_watts = rate(energy_mwh * 3.6, self.interval_seconds, 3);
        let battery_percent = rate(energy_mwh * 100.0, self.battery_full_mwh, 6);
        let battery_percent_per_hour = rate(power_watts * 100_000.0, self.battery_full_mwh, 4);
        EnergyUsage {
            energy_mwh,
            battery_percent,
            power_watts,
            battery_percent_per_hour,
            energy_source: self.energy_source.clone(),
        }
    }

    pub fn interval_seconds(&self) -> f64 {
        self.interval_seconds
    }
}

impl ResourceUsage {
    fn add_process(&mut self, process: &ProcessUsage) {
        self.compute.cpu_percent += process.cpu_percent;
        self.compute.memory_bytes = self
            .compute
            .memory_bytes
            .saturating_add(process.memory_bytes);
        self.compute.gpu_percent += process.gpu_percent;
        self.compute.gpu_memory_bytes = self
            .compute
            .gpu_memory_bytes
            .saturating_add(process.gpu_memory_bytes);
        self.storage.disk_read_bytes = self
            .storage
            .disk_read_bytes
            .saturating_add(process.disk_read_bytes);
        self.storage.disk_write_bytes = self
            .storage
            .disk_write_bytes
            .saturating_add(process.disk_write_bytes);
    }
}

impl ComputeUsage {
    fn normalize_cpu(&mut self, logical_cpus: usize) {
        let raw_cpu = self.cpu_percent.max(0.0);
        self.cpu_percent = rounded(raw_cpu, 1);
        self.cpu_percent_of_machine =
            rounded((raw_cpu / logical_cpus.max(1) as f64).clamp(0.0, 100.0), 1);
        self.gpu_percent = rounded(self.gpu_percent, 1);
    }
}

impl StorageUsage {
    fn normalize_rates(&mut self, seconds: f64) {
        self.disk_read_bytes_per_second = rate(self.disk_read_bytes as f64, seconds, 1);
        self.disk_write_bytes_per_second = rate(self.disk_write_bytes as f64, seconds, 1);
    }
}

mod energy;

use energy::EnergySampler;

#[derive(Debug, Default)]
pub struct ResourceSampler {
    previous_processes: HashMap<u32, PreviousProcess>,
    previous_gpu_engines: HashMap<(u32, u64, String), u64>,
    previous_system_ticks: Option<u64>,
    previous_sample: Option<Instant>,
    energy: EnergySampler,
}

impl ResourceSampler {
    pub fn sample_for_roots(
        &mut self,
        active_roots: impl IntoIterator<Item = u32>,
    ) -> ResourceSnapshot {
        let now = Instant::now();
        let interval_seconds = self
            .previous_sample
            .map(|previous| now.duration_since(previous).as_secs_f64())
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(0.0);
        let (system_ticks, logical_cpus) = read_system_cpu();
        let system_delta = self
            .previous_system_ticks
            .map(|previous| system_ticks.saturating_sub(previous))
            .filter(|delta| *delta > 0);
        let current = read_processes();
        let process_children = process_children(&current);
        let active_processes = descendants(active_roots, &process_children);
        let current_gpu = read_gpu_processes(&active_processes);
        let mut current_open_files = read_open_files(&active_processes);
        let energy = self.energy.sample(interval_seconds);
        let mut snapshot = ResourceSnapshot {
            logical_cpus,
            interval_seconds,
            system_energy_mwh: finite_nonnegative(energy.energy_mwh),
            battery_full_mwh: finite_nonnegative(energy.battery_full_mwh),
            energy_source: if energy.source.is_empty() {
                "unavailable".into()
            } else {
                energy.source
            },
            ..ResourceSnapshot::default()
        };

        let mut next_gpu_engines = HashMap::new();
        for (&pid, process) in &current {
            let cpu_percent = self.cpu_percent(pid, process, system_delta, logical_cpus);
            let (disk_read_bytes, disk_write_bytes) = self.disk_delta(pid, process);
            let gpu = current_gpu.get(&pid);
            let gpu_percent = self.gpu_percent(
                pid,
                process.start_ticks,
                gpu,
                interval_seconds,
                &mut next_gpu_engines,
            );
            snapshot.insert(
                pid,
                ProcessUsage {
                    parent_pid: process.parent_pid,
                    cpu_percent,
                    memory_bytes: process.memory_bytes,
                    gpu_percent,
                    gpu_memory_bytes: gpu.map_or(0, |gpu| gpu.memory_bytes),
                    disk_read_bytes,
                    disk_write_bytes,
                    open_files: current_open_files.remove(&pid).unwrap_or_default(),
                },
            );
        }
        self.remember(current, next_gpu_engines, system_ticks, now);
        snapshot
    }

    fn cpu_percent(
        &self,
        pid: u32,
        process: &ProcessStat,
        system_delta: Option<u64>,
        logical_cpus: usize,
    ) -> f64 {
        let Some((previous, total_delta)) = self.previous(pid, process).zip(system_delta) else {
            return 0.0;
        };
        finite_nonnegative(
            process.total_ticks.saturating_sub(previous.total_ticks) as f64 / total_delta as f64
                * logical_cpus as f64
                * 100.0,
        )
    }

    fn disk_delta(&self, pid: u32, process: &ProcessStat) -> (u64, u64) {
        self.previous(pid, process).map_or((0, 0), |previous| {
            (
                process
                    .disk_read_bytes
                    .saturating_sub(previous.disk_read_bytes),
                process
                    .disk_write_bytes
                    .saturating_sub(previous.disk_write_bytes),
            )
        })
    }

    fn previous(&self, pid: u32, process: &ProcessStat) -> Option<&PreviousProcess> {
        self.previous_processes
            .get(&pid)
            .filter(|previous| previous.start_ticks == process.start_ticks)
    }

    fn gpu_percent(
        &self,
        pid: u32,
        start_ticks: u64,
        gpu: Option<&GpuProcessStat>,
        seconds: f64,
        next: &mut HashMap<(u32, u64, String), u64>,
    ) -> f64 {
        let Some(gpu) = gpu.filter(|_| seconds > 0.0) else {
            return 0.0;
        };
        let elapsed = gpu
            .engine_nanoseconds
            .iter()
            .map(|(engine, &nanoseconds)| {
                let key = (pid, start_ticks, engine.clone());
                let previous = self.previous_gpu_engines.get(&key).copied();
                next.insert(key, nanoseconds);
                previous.map_or(0, |value| nanoseconds.saturating_sub(value))
            })
            .sum::<u64>();
        finite_nonnegative(elapsed as f64 / (seconds * 1_000_000_000.0) * 100.0)
    }

    fn remember(
        &mut self,
        current: HashMap<u32, ProcessStat>,
        gpu_engines: HashMap<(u32, u64, String), u64>,
        system_ticks: u64,
        now: Instant,
    ) {
        self.previous_processes = current
            .into_iter()
            .map(|(pid, process)| (pid, process.into()))
            .collect();
        self.previous_gpu_engines = gpu_engines;
        self.previous_system_ticks = Some(system_ticks);
        self.previous_sample = Some(now);
    }
}

impl From<ProcessStat> for PreviousProcess {
    fn from(process: ProcessStat) -> Self {
        Self {
            total_ticks: process.total_ticks,
            start_ticks: process.start_ticks,
            disk_read_bytes: process.disk_read_bytes,
            disk_write_bytes: process.disk_write_bytes,
        }
    }
}

impl ResourceSnapshot {
    fn insert(&mut self, pid: u32, process: ProcessUsage) {
        self.total_process_cpu_percent += process.cpu_percent;
        self.total_process_gpu_percent += process.gpu_percent;
        self.children
            .entry(process.parent_pid)
            .or_default()
            .push(pid);
        self.processes.insert(pid, process);
    }
}

fn read_processes() -> HashMap<u32, ProcessStat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let memory_bytes = resident_memory_bytes(&entry.path().join("status"));
            let (disk_read_bytes, disk_write_bytes) = read_process_io(&entry.path().join("io"));
            parse_process_stat(&stat, memory_bytes, disk_read_bytes, disk_write_bytes)
                .map(|process| (pid, process))
        })
        .collect()
}

fn process_children(processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, process) in processes {
        children.entry(process.parent_pid).or_default().push(pid);
    }
    children
}

fn descendants(
    roots: impl IntoIterator<Item = u32>,
    children: &HashMap<u32, Vec<u32>>,
) -> HashSet<u32> {
    let mut pending = roots.into_iter().filter(|pid| *pid > 0).collect::<Vec<_>>();
    let mut included = HashSet::new();
    while let Some(pid) = pending.pop() {
        if included.insert(pid)
            && let Some(process_children) = children.get(&pid)
        {
            pending.extend(process_children);
        }
    }
    included
}

fn read_open_files(pids: &HashSet<u32>) -> HashMap<u32, HashMap<DiskFileId, u64>> {
    pids.iter()
        .map(|&pid| (pid, read_process_open_files(pid)))
        .collect()
}

fn read_process_open_files(pid: u32) -> HashMap<DiskFileId, u64> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = fs::metadata(entry.path()).ok()?;
            metadata.file_type().is_file().then(|| {
                (
                    DiskFileId {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    metadata.blocks().saturating_mul(512),
                )
            })
        })
        .collect()
}

fn parse_process_stat(
    value: &str,
    memory_bytes: u64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
) -> Option<ProcessStat> {
    let fields = process_stat_fields(value)?;
    Some(ProcessStat {
        parent_pid: parse_field(&fields, 1)?,
        total_ticks: parse_field::<u64>(&fields, 11)?.saturating_add(parse_field(&fields, 12)?),
        start_ticks: parse_field(&fields, 19)?,
        memory_bytes,
        disk_read_bytes,
        disk_write_bytes,
    })
}

fn process_stat_fields(value: &str) -> Option<Vec<&str>> {
    let command_end = value.rfind(')')?;
    Some(value.get(command_end + 1..)?.split_whitespace().collect())
}

fn parse_field<T: std::str::FromStr>(fields: &[&str], index: usize) -> Option<T> {
    fields.get(index)?.parse().ok()
}

fn read_process_io(path: &Path) -> (u64, u64) {
    let Ok(value) = fs::read_to_string(path) else {
        return (0, 0);
    };
    let values = numeric_key_values(&value);
    (
        values.get("read_bytes").copied().unwrap_or(0),
        values.get("write_bytes").copied().unwrap_or(0),
    )
}

fn numeric_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| Some((key, value.trim().parse().ok()?)))
        .collect()
}

fn read_system_cpu() -> (u64, usize) {
    let Ok(stat) = fs::read_to_string("/proc/stat") else {
        return (0, 1);
    };
    let mut total = 0_u64;
    let mut logical_cpus = 0_usize;
    for line in stat.lines() {
        if let Some(values) = line.strip_prefix("cpu ") {
            let fields = values
                .split_whitespace()
                .filter_map(|value| value.parse::<u64>().ok())
                .collect::<Vec<_>>();
            // The first eight counters include steal but exclude guest and guest_nice,
            // which are already represented in user and nice.
            total = fields.iter().take(8).copied().sum::<u64>();
        } else if line
            .strip_prefix("cpu")
            .and_then(|value| value.split_whitespace().next())
            .is_some_and(|value| value.chars().all(|character| character.is_ascii_digit()))
        {
            logical_cpus += 1;
        }
    }
    (total, logical_cpus.max(1))
}

fn resident_memory_bytes(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        })
        .unwrap_or(0)
        .saturating_mul(1024)
}

fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn rate(numerator: f64, denominator: f64, decimals: i32) -> f64 {
    if denominator > 0.0 {
        rounded(numerator / denominator, decimals)
    } else {
        0.0
    }
}

fn rounded(value: f64, decimals: i32) -> f64 {
    let value = finite_nonnegative(value);
    let scale = 10_f64.powi(decimals);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::{DiskFileId, ProcessUsage, ResourceSnapshot, parse_process_stat};
    use anyhow::Context;
    use std::collections::HashMap;

    #[test]
    fn parses_proc_stat_with_spaces_in_command() -> anyhow::Result<()> {
        let stat = "42 (application helper) S 7 0 0 0 0 0 0 0 0 0 120 30 0 0 0 0 0 0 99 0 0";
        let process = parse_process_stat(stat, 4096, 8192, 16384).context("valid stat")?;
        assert_eq!(process.parent_pid, 7);
        assert_eq!(process.total_ticks, 150);
        assert_eq!(process.start_ticks, 99);
        assert_eq!(process.memory_bytes, 4096);
        assert_eq!(process.disk_read_bytes, 8192);
        assert_eq!(process.disk_write_bytes, 16384);
        Ok(())
    }

    #[test]
    fn totals_process_trees_without_double_counting_shared_roots() {
        let file = |inode, bytes| (DiskFileId { device: 1, inode }, bytes);
        let process = |parent_pid,
                       cpu_percent,
                       memory_bytes,
                       disk_read_bytes,
                       disk_write_bytes,
                       open_files| ProcessUsage {
            parent_pid,
            cpu_percent,
            memory_bytes,
            gpu_percent: 0.0,
            gpu_memory_bytes: 0,
            disk_read_bytes,
            disk_write_bytes,
            open_files,
        };
        let processes = HashMap::from([
            (
                10,
                process(
                    1,
                    2.0,
                    100,
                    10,
                    20,
                    HashMap::from([file(1, 1024), file(2, 2048)]),
                ),
            ),
            (
                11,
                process(10, 3.5, 200, 30, 40, HashMap::from([file(1, 1024)])),
            ),
            (20, process(1, 1.0, 50, 50, 60, HashMap::new())),
        ]);
        let children = HashMap::from([(1, vec![10, 20]), (10, vec![11])]);
        let snapshot = ResourceSnapshot {
            processes,
            children,
            logical_cpus: 4,
            total_process_cpu_percent: 10.0,
            total_process_gpu_percent: 0.0,
            interval_seconds: 2.0,
            system_energy_mwh: 5.0,
            battery_full_mwh: 50_000.0,
            energy_source: "rapl".into(),
        };
        let usage = snapshot.usage_for_roots([10, 10, 11]);
        assert_eq!(usage.compute.cpu_percent, 5.5);
        assert_eq!(usage.compute.cpu_percent_of_machine, 1.4);
        assert_eq!(usage.compute.memory_bytes, 300);
        assert_eq!(usage.storage.disk_read_bytes, 40);
        assert_eq!(usage.storage.disk_write_bytes, 60);
        assert_eq!(usage.storage.disk_read_bytes_per_second, 20.0);
        assert_eq!(usage.storage.disk_write_bytes_per_second, 30.0);
        assert_eq!(usage.storage.open_file_disk_bytes, 3072);
        assert_eq!(usage.energy.energy_mwh, 2.75);
        assert_eq!(usage.energy.energy_source, "rapl");
    }

    #[test]
    fn ignores_previous_counters_after_pid_reuse() {
        let sampler = super::ResourceSampler {
            previous_processes: HashMap::from([(
                42,
                super::PreviousProcess {
                    total_ticks: 100,
                    start_ticks: 7,
                    disk_read_bytes: 1_000,
                    disk_write_bytes: 2_000,
                },
            )]),
            ..Default::default()
        };
        let process = super::ProcessStat {
            parent_pid: 1,
            total_ticks: 500,
            start_ticks: 8,
            memory_bytes: 0,
            disk_read_bytes: 4_000,
            disk_write_bytes: 8_000,
        };
        assert_eq!(sampler.cpu_percent(42, &process, Some(100), 4), 0.0);
        assert_eq!(sampler.disk_delta(42, &process), (0, 0));
    }
}
