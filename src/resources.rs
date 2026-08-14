use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage};

#[derive(Debug, Clone, Copy)]
struct ProcessStat {
    parent_pid: u32,
    total_ticks: u64,
    start_ticks: u64,
    major_faults: u64,
    thread_count: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcessIo {
    physical_read_bytes: u64,
    physical_write_bytes: u64,
    logical_read_bytes: u64,
    logical_write_bytes: u64,
    read_operations: u64,
    write_operations: u64,
    cancelled_write_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct PreviousProcess {
    total_ticks: u64,
    start_ticks: u64,
    major_faults: u64,
    io: Option<ProcessIo>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CgroupCounters {
    cpu_usage_usec: u64,
    read_bytes: u64,
    write_bytes: u64,
    read_operations: u64,
    write_operations: u64,
    memory_bytes: u64,
    swap_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct CgroupUsage {
    cpu_percent: f64,
    read_bytes: u64,
    write_bytes: u64,
    read_operations: u64,
    write_operations: u64,
    memory_bytes: u64,
    swap_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct MemoryUsage {
    rss_bytes: u64,
    pss_bytes: u64,
    private_bytes: u64,
    swap_bytes: u64,
    pss_available: bool,
}

#[derive(Debug, Clone)]
struct ProcessUsage {
    parent_pid: u32,
    cpu_percent: f64,
    memory: MemoryUsage,
    thread_count: u64,
    major_faults: u64,
    gpu_available: bool,
    gpu_percent: f64,
    gpu_busy_percent: f64,
    gpu_memory_resident_bytes: u64,
    gpu_memory_allocated_bytes: u64,
    io: ProcessIo,
    open_files: HashMap<DiskFileId, DiskFile>,
    referenced_files: HashMap<DiskFileId, DiskFile>,
    network_sockets: HashSet<u64>,
    storage_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiskFileId {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy)]
struct DiskFile {
    bytes: u64,
    temporary: bool,
}

mod gpu;

use gpu::{GpuProcessStat, read_gpu_processes};

#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    processes: HashMap<u32, ProcessUsage>,
    children: HashMap<u32, Vec<u32>>,
    cgroup_members_by_root: HashMap<u32, HashSet<u32>>,
    cgroup_path_by_root: HashMap<u32, String>,
    cgroup_usage: HashMap<String, CgroupUsage>,
    app_disk_by_target: HashMap<String, DiskBreakdown>,
    shared_pids: HashSet<u32>,
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
            cgroup_members_by_root: HashMap::new(),
            cgroup_path_by_root: HashMap::new(),
            cgroup_usage: HashMap::new(),
            app_disk_by_target: HashMap::new(),
            shared_pids: HashSet::new(),
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
    pub fn usage_for_target(
        &self,
        target_id: &str,
        roots: impl IntoIterator<Item = u32>,
    ) -> ResourceUsage {
        let mut usage = self.usage_for_roots(roots);
        if let Some(disk) = self.app_disk_by_target.get(target_id) {
            usage.storage.disk_space_total_bytes = disk.total_bytes;
            usage.storage.disk_space_temporary_bytes = disk.temporary_bytes;
            usage.storage.disk_space_permanent_bytes = disk.permanent_bytes;
            usage.measurement.disk_space_scope = "identified-app-directories".into();
        } else {
            usage.measurement.disk_space_scope = "unavailable".into();
        }
        usage
    }

    pub fn usage_for_roots(&self, roots: impl IntoIterator<Item = u32>) -> ResourceUsage {
        let roots = roots
            .into_iter()
            .filter(|pid| *pid > 0)
            .collect::<HashSet<_>>();
        let mut pids = HashSet::new();
        let mut cgroup_roots = 0_usize;
        let mut cgroup_paths = HashSet::new();
        for root in &roots {
            if let Some(members) = self.cgroup_members_by_root.get(root) {
                pids.extend(members);
                if let Some(path) = self.cgroup_path_by_root.get(root) {
                    cgroup_paths.insert(path);
                }
                cgroup_roots += 1;
            } else {
                pids.extend(descendants([*root], &self.children));
            }
        }
        let mut usage = ResourceUsage::default();
        let mut open_files = HashMap::new();
        let mut referenced_files = HashMap::new();
        let mut network_sockets = HashSet::<u64>::new();
        let mut covered_processes = 0_u64;
        let mut pss_processes = 0_u64;
        let mut gpu_processes = 0_u64;
        for process in pids.iter().filter_map(|pid| self.processes.get(pid)) {
            usage.add_process(process);
            covered_processes += 1;
            pss_processes += u64::from(process.memory.pss_available);
            gpu_processes += u64::from(process.gpu_available);
            merge_disk_files(&mut open_files, &process.open_files);
            merge_disk_files(&mut referenced_files, &process.referenced_files);
            network_sockets.extend(process.network_sockets.iter().copied());
        }
        if !roots.is_empty() && cgroup_roots == roots.len() {
            let mut cgroup = CgroupUsage::default();
            for usage in cgroup_paths
                .iter()
                .filter_map(|path| self.cgroup_usage.get(*path))
            {
                cgroup.cpu_percent += usage.cpu_percent;
                cgroup.read_bytes = cgroup.read_bytes.saturating_add(usage.read_bytes);
                cgroup.write_bytes = cgroup.write_bytes.saturating_add(usage.write_bytes);
                cgroup.read_operations =
                    cgroup.read_operations.saturating_add(usage.read_operations);
                cgroup.write_operations = cgroup
                    .write_operations
                    .saturating_add(usage.write_operations);
                cgroup.memory_bytes = cgroup.memory_bytes.saturating_add(usage.memory_bytes);
                cgroup.swap_bytes = cgroup.swap_bytes.saturating_add(usage.swap_bytes);
            }
            usage.compute.cpu_percent = cgroup.cpu_percent;
            usage.compute.memory_cgroup_bytes = cgroup.memory_bytes;
            usage.storage.disk_read_bytes = cgroup.read_bytes;
            usage.storage.disk_write_bytes = cgroup.write_bytes;
            usage.storage.read_operations = cgroup.read_operations;
            usage.storage.write_operations = cgroup.write_operations;
        }
        usage.storage.open_file_disk_bytes = open_files.values().map(|file| file.bytes).sum();
        for file in referenced_files.values() {
            usage.storage.referenced_file_disk_bytes = usage
                .storage
                .referenced_file_disk_bytes
                .saturating_add(file.bytes);
            if file.temporary {
                usage.storage.referenced_file_temporary_bytes = usage
                    .storage
                    .referenced_file_temporary_bytes
                    .saturating_add(file.bytes);
            } else {
                usage.storage.referenced_file_permanent_bytes = usage
                    .storage
                    .referenced_file_permanent_bytes
                    .saturating_add(file.bytes);
            }
        }
        usage.measurement.sample_interval_ms = (self.interval_seconds * 1000.0).round() as u64;
        usage.measurement.attribution_method = if !roots.is_empty() && cgroup_roots == roots.len() {
            "cgroup".into()
        } else if cgroup_roots > 0 {
            "mixed".into()
        } else {
            "process-tree".into()
        };
        usage.measurement.coverage = if !roots.is_empty() && cgroup_roots == roots.len() {
            1.0
        } else if pids.is_empty() {
            0.0
        } else {
            covered_processes as f64 / pids.len() as f64
        };
        usage.measurement.memory_source =
            if covered_processes > 0 && pss_processes == covered_processes {
                "pss".into()
            } else if covered_processes > 0 {
                "rss-fallback".into()
            } else {
                "unavailable".into()
            };
        usage.measurement.gpu_available = gpu_processes > 0;
        usage.measurement.storage_available = (!cgroup_paths.is_empty()
            && cgroup_roots == roots.len())
            || pids
                .iter()
                .filter_map(|pid| self.processes.get(pid))
                .any(|process| process.storage_available);
        usage.network.network_connection_count = network_sockets.len() as u64;
        usage.measurement.network_available = covered_processes > 0;
        usage.measurement.network_bytes_available = false;
        usage.measurement.network_connections_available = covered_processes > 0;
        usage.measurement.resources_shared = pids.iter().any(|pid| self.shared_pids.contains(pid));
        self.complete(usage)
    }

    fn complete(&self, mut usage: ResourceUsage) -> ResourceUsage {
        usage.compute.major_faults_per_second = rate(
            usage.compute.major_faults_per_second,
            self.interval_seconds,
            2,
        );
        usage.compute.normalize_cpu(self.logical_cpus);
        usage.storage.normalize_rates(self.interval_seconds);
        usage.energy =
            self.estimated_energy(usage.compute.cpu_percent, self.total_process_cpu_percent);
        usage
    }

    fn estimated_energy(&self, activity: f64, total: f64) -> EnergyUsage {
        let share = if total > 0.0 {
            (activity / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let attributable = self.energy_source == "rapl";
        let attributed_fraction = if attributable { share } else { 0.0 };
        let energy_mwh = if attributable {
            rounded(self.system_energy_mwh * attributed_fraction, 4)
        } else {
            0.0
        };
        let power_watts = rate(energy_mwh * 3.6, self.interval_seconds, 3);
        let system_power_watts = rate(self.system_energy_mwh * 3.6, self.interval_seconds, 3);
        let battery_percent = rate(energy_mwh * 100.0, self.battery_full_mwh, 6);
        let battery_percent_per_hour = rate(power_watts * 100_000.0, self.battery_full_mwh, 4);
        EnergyUsage {
            energy_mwh,
            battery_percent,
            power_watts,
            estimated_app_power_watts: power_watts,
            system_power_watts,
            battery_percent_per_hour,
            attributed_fraction: rounded(attributed_fraction, 4),
            energy_source: self.energy_source.clone(),
            energy_confidence: if attributable {
                "low".into()
            } else {
                "system-only".into()
            },
        }
    }

    pub fn interval_seconds(&self) -> f64 {
        self.interval_seconds
    }
}

impl ResourceUsage {
    fn add_process(&mut self, process: &ProcessUsage) {
        self.compute.cpu_percent += process.cpu_percent;
        let effective_memory = if process.memory.pss_available {
            process.memory.pss_bytes
        } else {
            process.memory.rss_bytes
        };
        self.compute.memory_bytes = self.compute.memory_bytes.saturating_add(effective_memory);
        self.compute.memory_rss_bytes = self
            .compute
            .memory_rss_bytes
            .saturating_add(process.memory.rss_bytes);
        self.compute.memory_pss_bytes = self
            .compute
            .memory_pss_bytes
            .saturating_add(process.memory.pss_bytes);
        self.compute.memory_private_bytes = self
            .compute
            .memory_private_bytes
            .saturating_add(process.memory.private_bytes);
        self.compute.memory_swap_bytes = self
            .compute
            .memory_swap_bytes
            .saturating_add(process.memory.swap_bytes);
        self.compute.process_count = self.compute.process_count.saturating_add(1);
        self.compute.thread_count = self
            .compute
            .thread_count
            .saturating_add(process.thread_count);
        self.compute.major_faults_per_second += process.major_faults as f64;
        self.compute.gpu_percent += process.gpu_percent;
        self.compute.gpu_busy_percent = self.compute.gpu_busy_percent.max(process.gpu_busy_percent);
        self.compute.gpu_memory_resident_bytes = self
            .compute
            .gpu_memory_resident_bytes
            .saturating_add(process.gpu_memory_resident_bytes);
        self.compute.gpu_memory_allocated_bytes = self
            .compute
            .gpu_memory_allocated_bytes
            .saturating_add(process.gpu_memory_allocated_bytes);
        self.compute.gpu_memory_bytes = self.compute.gpu_memory_bytes.saturating_add(
            if process.gpu_memory_resident_bytes > 0 {
                process.gpu_memory_resident_bytes
            } else {
                process.gpu_memory_allocated_bytes
            },
        );
        self.storage.disk_read_bytes = self
            .storage
            .disk_read_bytes
            .saturating_add(process.io.physical_read_bytes);
        self.storage.disk_write_bytes = self
            .storage
            .disk_write_bytes
            .saturating_add(process.io.physical_write_bytes);
        self.storage.logical_read_bytes = self
            .storage
            .logical_read_bytes
            .saturating_add(process.io.logical_read_bytes);
        self.storage.logical_write_bytes = self
            .storage
            .logical_write_bytes
            .saturating_add(process.io.logical_write_bytes);
        self.storage.read_operations = self
            .storage
            .read_operations
            .saturating_add(process.io.read_operations);
        self.storage.write_operations = self
            .storage
            .write_operations
            .saturating_add(process.io.write_operations);
        self.storage.cancelled_write_bytes = self
            .storage
            .cancelled_write_bytes
            .saturating_add(process.io.cancelled_write_bytes);
    }
}

impl ComputeUsage {
    fn normalize_cpu(&mut self, logical_cpus: usize) {
        let raw_cpu = self.cpu_percent.max(0.0);
        self.cpu_percent = rounded(raw_cpu, 1);
        self.cpu_percent_of_machine =
            rounded((raw_cpu / logical_cpus.max(1) as f64).clamp(0.0, 100.0), 1);
        self.gpu_percent = rounded(self.gpu_percent, 1);
        self.gpu_busy_percent = rounded(self.gpu_busy_percent.clamp(0.0, 100.0), 1);
    }
}

impl StorageUsage {
    fn normalize_rates(&mut self, seconds: f64) {
        self.disk_read_bytes_per_second = rate(self.disk_read_bytes as f64, seconds, 1);
        self.disk_write_bytes_per_second = rate(self.disk_write_bytes as f64, seconds, 1);
        self.logical_read_bytes_per_second = rate(self.logical_read_bytes as f64, seconds, 1);
        self.logical_write_bytes_per_second = rate(self.logical_write_bytes as f64, seconds, 1);
        self.read_operations_per_second = rate(self.read_operations as f64, seconds, 1);
        self.write_operations_per_second = rate(self.write_operations as f64, seconds, 1);
    }
}

mod energy;

use energy::EnergySampler;

#[derive(Debug, Default)]
pub struct ResourceSampler {
    previous_processes: HashMap<u32, PreviousProcess>,
    previous_gpu_engines: HashMap<(u32, u64, String), u64>,
    previous_system_ticks: Option<u64>,
    previous_cgroups: HashMap<String, CgroupCounters>,
    previous_sample: Option<Instant>,
    open_files: OpenFileCache,
    app_disk: AppDiskCache,
    energy: EnergySampler,
}

#[derive(Debug, Default)]
struct OpenFileCache {
    samples: HashMap<u32, ProcessFiles>,
    next_refresh: InstantSlot,
}

#[derive(Debug, Clone, Default)]
struct ProcessFiles {
    open: HashMap<DiskFileId, DiskFile>,
    referenced: HashMap<DiskFileId, DiskFile>,
    sockets: HashSet<u64>,
}

#[derive(Debug, Default)]
struct InstantSlot(Option<Instant>);

#[derive(Debug, Clone, Copy, Default)]
struct DiskBreakdown {
    total_bytes: u64,
    temporary_bytes: u64,
    permanent_bytes: u64,
}

#[derive(Debug, Default)]
struct AppDiskCache {
    samples: HashMap<String, DiskBreakdown>,
    next_refresh: InstantSlot,
}

impl ResourceSampler {
    pub fn sample_for_targets(
        &mut self,
        active_targets: &HashMap<String, Vec<u32>>,
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
        let active_roots = active_targets
            .values()
            .flatten()
            .copied()
            .filter(|pid| *pid > 0)
            .collect::<HashSet<_>>();
        let cgroup_path_by_root = cgroup_paths_for_roots(&active_roots);
        let cgroup_members_by_root = cgroup_members_for_paths(&cgroup_path_by_root);
        let current_cgroups = cgroup_path_by_root
            .values()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|path| (path.clone(), read_cgroup_counters(path)))
            .collect::<HashMap<_, _>>();
        let cgroup_usage = self.cgroup_usage(&current_cgroups, interval_seconds);
        let mut active_processes = descendants(active_roots.iter().copied(), &process_children);
        for members in cgroup_members_by_root.values() {
            active_processes.extend(members);
        }
        let current_gpu = read_gpu_processes(&active_processes);
        let mut current_open_files = self.open_files.read(&active_processes, now);
        let energy = self.energy.sample(interval_seconds);
        let app_disk_by_target = self.app_disk.read(active_targets.keys(), now);
        let shared_pids =
            shared_target_pids(active_targets, &process_children, &cgroup_members_by_root);
        let mut snapshot = ResourceSnapshot {
            cgroup_members_by_root,
            cgroup_path_by_root,
            cgroup_usage,
            app_disk_by_target,
            shared_pids,
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
        let mut current_io = HashMap::new();
        for (&pid, process) in &current {
            let active = active_processes.contains(&pid);
            let cpu_percent = self.cpu_percent(pid, process, system_delta, logical_cpus);
            let memory = if active {
                read_process_memory(pid)
            } else {
                MemoryUsage::default()
            };
            let sampled_io = active.then(|| read_process_io(pid)).flatten();
            let io = self.io_delta(pid, process, sampled_io.unwrap_or_default());
            if let Some(value) = sampled_io {
                current_io.insert(pid, value);
            }
            let major_faults = self.major_fault_delta(pid, process);
            let gpu = current_gpu.get(&pid);
            let (gpu_percent, gpu_busy_percent) = self.gpu_percent(
                pid,
                process.start_ticks,
                gpu,
                interval_seconds,
                &mut next_gpu_engines,
            );
            let files = current_open_files.remove(&pid).unwrap_or_default();
            snapshot.insert(
                pid,
                ProcessUsage {
                    parent_pid: process.parent_pid,
                    cpu_percent,
                    memory,
                    thread_count: process.thread_count,
                    major_faults,
                    gpu_available: gpu.is_some(),
                    gpu_percent,
                    gpu_busy_percent,
                    gpu_memory_resident_bytes: gpu.map_or(0, |gpu| gpu.resident_memory_bytes),
                    gpu_memory_allocated_bytes: gpu.map_or(0, |gpu| gpu.allocated_memory_bytes),
                    io,
                    open_files: files.open,
                    referenced_files: files.referenced,
                    network_sockets: files.sockets,
                    storage_available: sampled_io.is_some(),
                },
            );
        }
        self.remember(
            current,
            current_io,
            current_cgroups,
            next_gpu_engines,
            system_ticks,
            now,
        );
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

    fn io_delta(&self, pid: u32, process: &ProcessStat, current: ProcessIo) -> ProcessIo {
        self.previous(pid, process)
            .and_then(|previous| previous.io)
            .map_or_else(ProcessIo::default, |previous| ProcessIo {
                physical_read_bytes: current
                    .physical_read_bytes
                    .saturating_sub(previous.physical_read_bytes),
                physical_write_bytes: current
                    .physical_write_bytes
                    .saturating_sub(previous.physical_write_bytes),
                logical_read_bytes: current
                    .logical_read_bytes
                    .saturating_sub(previous.logical_read_bytes),
                logical_write_bytes: current
                    .logical_write_bytes
                    .saturating_sub(previous.logical_write_bytes),
                read_operations: current
                    .read_operations
                    .saturating_sub(previous.read_operations),
                write_operations: current
                    .write_operations
                    .saturating_sub(previous.write_operations),
                cancelled_write_bytes: current
                    .cancelled_write_bytes
                    .saturating_sub(previous.cancelled_write_bytes),
            })
    }

    fn major_fault_delta(&self, pid: u32, process: &ProcessStat) -> u64 {
        self.previous(pid, process).map_or(0, |previous| {
            process.major_faults.saturating_sub(previous.major_faults)
        })
    }

    fn cgroup_usage(
        &self,
        current: &HashMap<String, CgroupCounters>,
        seconds: f64,
    ) -> HashMap<String, CgroupUsage> {
        current
            .iter()
            .map(|(path, counters)| {
                let usage =
                    self.previous_cgroups
                        .get(path)
                        .map_or_else(CgroupUsage::default, |previous| CgroupUsage {
                            cpu_percent: rate(
                                counters
                                    .cpu_usage_usec
                                    .saturating_sub(previous.cpu_usage_usec)
                                    as f64,
                                seconds * 10_000.0,
                                1,
                            ),
                            read_bytes: counters.read_bytes.saturating_sub(previous.read_bytes),
                            write_bytes: counters.write_bytes.saturating_sub(previous.write_bytes),
                            read_operations: counters
                                .read_operations
                                .saturating_sub(previous.read_operations),
                            write_operations: counters
                                .write_operations
                                .saturating_sub(previous.write_operations),
                            memory_bytes: counters.memory_bytes,
                            swap_bytes: counters.swap_bytes,
                        });
                (path.clone(), usage)
            })
            .collect()
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
    ) -> (f64, f64) {
        let Some(gpu) = gpu.filter(|_| seconds > 0.0) else {
            return (0.0, 0.0);
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
            .collect::<Vec<_>>();
        let denominator = seconds * 1_000_000_000.0;
        let aggregate = elapsed.iter().copied().sum::<u64>() as f64 / denominator * 100.0;
        let busiest = elapsed.iter().copied().max().unwrap_or(0) as f64 / denominator * 100.0;
        (
            finite_nonnegative(aggregate),
            finite_nonnegative(busiest).min(100.0),
        )
    }

    fn remember(
        &mut self,
        current: HashMap<u32, ProcessStat>,
        current_io: HashMap<u32, ProcessIo>,
        current_cgroups: HashMap<String, CgroupCounters>,
        gpu_engines: HashMap<(u32, u64, String), u64>,
        system_ticks: u64,
        now: Instant,
    ) {
        self.previous_processes = current
            .into_iter()
            .map(|(pid, process)| {
                let io = current_io.get(&pid).copied();
                (
                    pid,
                    PreviousProcess {
                        total_ticks: process.total_ticks,
                        start_ticks: process.start_ticks,
                        major_faults: process.major_faults,
                        io,
                    },
                )
            })
            .collect();
        self.previous_gpu_engines = gpu_engines;
        self.previous_cgroups = current_cgroups;
        self.previous_system_ticks = Some(system_ticks);
        self.previous_sample = Some(now);
    }
}

impl OpenFileCache {
    fn read(&mut self, pids: &HashSet<u32>, now: Instant) -> HashMap<u32, ProcessFiles> {
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples.retain(|pid, _| pids.contains(pid));
        if refresh {
            self.samples = read_process_files(pids);
            self.next_refresh.0 = Some(now + std::time::Duration::from_secs(10));
        } else {
            let missing = pids
                .iter()
                .filter(|pid| !self.samples.contains_key(pid))
                .copied()
                .collect::<Vec<_>>();
            for pid in missing {
                self.samples.insert(pid, read_process_file_sets(pid));
            }
        }
        self.samples.clone()
    }
}

impl AppDiskCache {
    fn read<'a>(
        &mut self,
        targets: impl IntoIterator<Item = &'a String>,
        now: Instant,
    ) -> HashMap<String, DiskBreakdown> {
        let targets = targets.into_iter().cloned().collect::<HashSet<_>>();
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples.retain(|target, _| targets.contains(target));
        if refresh {
            self.samples = targets
                .into_iter()
                .map(|target| {
                    let usage = application_disk_usage(&target);
                    (target, usage)
                })
                .collect();
            self.next_refresh.0 = Some(now + std::time::Duration::from_secs(30));
        } else {
            let missing = targets
                .into_iter()
                .filter(|target| !self.samples.contains_key(target))
                .collect::<Vec<_>>();
            for target in missing {
                self.samples
                    .insert(target.clone(), application_disk_usage(&target));
            }
        }
        self.samples.clone()
    }
}

fn application_disk_usage(target_id: &str) -> DiskBreakdown {
    let target = target_id.trim_end_matches(".desktop");
    if target.is_empty() || target.starts_with("window-group:") {
        return DiskBreakdown::default();
    }
    let lowercase = target.to_ascii_lowercase();
    let mut names = HashSet::from([target.to_owned(), lowercase.clone()]);
    for candidate in [target, &lowercase] {
        if let Some(short) = candidate.rsplit('.').next().filter(|name| name.len() >= 4) {
            names.insert(short.to_owned());
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = xdg_directory(
        "XDG_DATA_HOME",
        home.as_ref().map(|path| path.join(".local/share")),
    );
    let config = xdg_directory(
        "XDG_CONFIG_HOME",
        home.as_ref().map(|path| path.join(".config")),
    );
    let state = xdg_directory(
        "XDG_STATE_HOME",
        home.as_ref().map(|path| path.join(".local/state")),
    );
    let cache = xdg_directory(
        "XDG_CACHE_HOME",
        home.as_ref().map(|path| path.join(".cache")),
    );
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut permanent_roots = Vec::new();
    let mut temporary_roots = Vec::new();
    for name in names {
        for root in [&data, &config, &state].into_iter().flatten() {
            permanent_roots.push(root.join(&name));
        }
        for root in [&cache, &runtime].into_iter().flatten() {
            temporary_roots.push(root.join(&name));
        }
    }
    if let Some(home) = home {
        for name in [target, &lowercase] {
            let flatpak = home.join(".var/app").join(name);
            permanent_roots.extend([flatpak.join("config"), flatpak.join("data")]);
            temporary_roots.push(flatpak.join("cache"));
        }
    }
    let permanent = allocated_directory_bytes(&permanent_roots);
    let temporary = allocated_directory_bytes(&temporary_roots);
    DiskBreakdown {
        total_bytes: permanent.saturating_add(temporary),
        temporary_bytes: temporary,
        permanent_bytes: permanent,
    }
}

fn xdg_directory(variable: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from).or(fallback)
}

fn allocated_directory_bytes(roots: &[PathBuf]) -> u64 {
    let mut files = HashMap::<DiskFileId, u64>::new();
    for root in roots.iter().filter(|path| path.is_dir()) {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files
                .entry(DiskFileId {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
                .or_insert_with(|| metadata.blocks().saturating_mul(512));
        }
    }
    files.values().copied().sum()
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
            parse_process_stat(&stat).map(|process| (pid, process))
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

fn shared_target_pids(
    targets: &HashMap<String, Vec<u32>>,
    children: &HashMap<u32, Vec<u32>>,
    cgroups: &HashMap<u32, HashSet<u32>>,
) -> HashSet<u32> {
    let mut owners = HashMap::<u32, u32>::new();
    for roots in targets.values() {
        let mut target_pids = HashSet::new();
        for root in roots {
            if let Some(members) = cgroups.get(root) {
                target_pids.extend(members);
            } else {
                target_pids.extend(descendants([*root], children));
            }
        }
        for pid in target_pids {
            *owners.entry(pid).or_default() += 1;
        }
    }
    owners
        .into_iter()
        .filter_map(|(pid, owners)| (owners > 1).then_some(pid))
        .collect()
}

fn cgroup_paths_for_roots(roots: &HashSet<u32>) -> HashMap<u32, String> {
    roots
        .iter()
        .filter_map(|&root| {
            process_cgroup(root)
                .filter(|path| specific_application_cgroup(path))
                .map(|path| (root, path))
        })
        .collect()
}

fn cgroup_members_for_paths(paths: &HashMap<u32, String>) -> HashMap<u32, HashSet<u32>> {
    let mut by_path = HashMap::<String, HashSet<u32>>::new();
    paths
        .iter()
        .filter_map(|(&root, path)| {
            let members = by_path
                .entry(path.clone())
                .or_insert_with_key(|path| read_cgroup_members(path))
                .clone();
            (!members.is_empty()).then_some((root, members))
        })
        .collect()
}

fn process_cgroup(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
}

fn specific_application_cgroup(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    (name.ends_with(".scope") || name.ends_with(".service"))
        && (name.starts_with("app-") || name.contains("flatpak") || name.contains("snap."))
}

fn read_cgroup_counters(path: &str) -> CgroupCounters {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let cpu = fs::read_to_string(root.join("cpu.stat"))
        .ok()
        .map(|value| whitespace_key_values(&value))
        .unwrap_or_default();
    let mut counters = CgroupCounters {
        cpu_usage_usec: cpu.get("usage_usec").copied().unwrap_or(0),
        memory_bytes: read_number(&root.join("memory.current")),
        swap_bytes: read_number(&root.join("memory.swap.current")),
        ..CgroupCounters::default()
    };
    if let Ok(io) = fs::read_to_string(root.join("io.stat")) {
        for values in io.lines().map(equals_key_values) {
            counters.read_bytes = counters
                .read_bytes
                .saturating_add(values.get("rbytes").copied().unwrap_or(0));
            counters.write_bytes = counters
                .write_bytes
                .saturating_add(values.get("wbytes").copied().unwrap_or(0));
            counters.read_operations = counters
                .read_operations
                .saturating_add(values.get("rios").copied().unwrap_or(0));
            counters.write_operations = counters
                .write_operations
                .saturating_add(values.get("wios").copied().unwrap_or(0));
        }
    }
    counters
}

fn whitespace_key_values(value: &str) -> HashMap<String, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .filter_map(|(key, value)| Some((key.to_owned(), value.trim().parse().ok()?)))
        .collect()
}

fn equals_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .filter_map(|(key, value)| Some((key, value.parse().ok()?)))
        .collect()
}

fn read_number(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn read_cgroup_members(path: &str) -> HashSet<u32> {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "cgroup.procs")
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|value| {
            value
                .lines()
                .filter_map(|line| line.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
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

fn read_process_files(pids: &HashSet<u32>) -> HashMap<u32, ProcessFiles> {
    pids.iter()
        .map(|&pid| (pid, read_process_file_sets(pid)))
        .collect()
}

fn read_process_file_sets(pid: u32) -> ProcessFiles {
    let fd_directory = format!("/proc/{pid}/fd");
    let open = read_regular_files(&fd_directory);
    let sockets = read_socket_inodes(&fd_directory);
    let mut referenced = open.clone();
    merge_disk_files(
        &mut referenced,
        &read_regular_files(&format!("/proc/{pid}/map_files")),
    );
    ProcessFiles {
        open,
        referenced,
        sockets,
    }
}

fn read_socket_inodes(directory: &str) -> HashSet<u64> {
    let Ok(entries) = fs::read_dir(directory) else {
        return HashSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter_map(|target| {
            let value = target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?;
            value.parse().ok()
        })
        .collect()
}

fn read_regular_files(directory: &str) -> HashMap<DiskFileId, DiskFile> {
    let Ok(entries) = fs::read_dir(directory) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let link = fs::read_link(entry.path()).ok()?;
            let metadata = fs::metadata(entry.path()).ok()?;
            metadata.file_type().is_file().then(|| {
                (
                    DiskFileId {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    DiskFile {
                        bytes: metadata.blocks().saturating_mul(512),
                        temporary: temporary_path(&link),
                    },
                )
            })
        })
        .collect()
}

fn merge_disk_files(
    target: &mut HashMap<DiskFileId, DiskFile>,
    source: &HashMap<DiskFileId, DiskFile>,
) {
    for (&id, &file) in source {
        target
            .entry(id)
            .and_modify(|current| {
                current.bytes = current.bytes.max(file.bytes);
                current.temporary |= file.temporary;
            })
            .or_insert(file);
    }
}

fn temporary_path(path: &Path) -> bool {
    path.starts_with("/tmp")
        || path.starts_with("/var/tmp")
        || path.starts_with("/dev/shm")
        || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|root| path.starts_with(root))
        || std::env::var_os("XDG_CACHE_HOME").is_some_and(|root| path.starts_with(root))
        || std::env::var_os("HOME")
            .is_some_and(|home| path.starts_with(Path::new(&home).join(".cache")))
}

fn parse_process_stat(value: &str) -> Option<ProcessStat> {
    let fields = process_stat_fields(value)?;
    Some(ProcessStat {
        parent_pid: parse_field(&fields, 1)?,
        total_ticks: parse_field::<u64>(&fields, 11)?.saturating_add(parse_field(&fields, 12)?),
        start_ticks: parse_field(&fields, 19)?,
        major_faults: parse_field(&fields, 9)?,
        thread_count: parse_field(&fields, 17)?,
    })
}

fn process_stat_fields(value: &str) -> Option<Vec<&str>> {
    let command_end = value.rfind(')')?;
    Some(value.get(command_end + 1..)?.split_whitespace().collect())
}

fn parse_field<T: std::str::FromStr>(fields: &[&str], index: usize) -> Option<T> {
    fields.get(index)?.parse().ok()
}

fn read_process_io(pid: u32) -> Option<ProcessIo> {
    let value = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let values = numeric_key_values(&value);
    Some(ProcessIo {
        physical_read_bytes: values.get("read_bytes").copied().unwrap_or(0),
        physical_write_bytes: values.get("write_bytes").copied().unwrap_or(0),
        logical_read_bytes: values.get("rchar").copied().unwrap_or(0),
        logical_write_bytes: values.get("wchar").copied().unwrap_or(0),
        read_operations: values.get("syscr").copied().unwrap_or(0),
        write_operations: values.get("syscw").copied().unwrap_or(0),
        cancelled_write_bytes: values.get("cancelled_write_bytes").copied().unwrap_or(0),
    })
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

fn read_process_memory(pid: u32) -> MemoryUsage {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .map(|value| memory_key_values(&value));
    if let Some(values) = rollup {
        let private_kib = values
            .get("Private_Clean")
            .copied()
            .unwrap_or(0)
            .saturating_add(values.get("Private_Dirty").copied().unwrap_or(0))
            .saturating_add(values.get("Private_Hugetlb").copied().unwrap_or(0));
        return MemoryUsage {
            rss_bytes: values.get("Rss").copied().unwrap_or(0).saturating_mul(1024),
            pss_bytes: values.get("Pss").copied().unwrap_or(0).saturating_mul(1024),
            private_bytes: private_kib.saturating_mul(1024),
            swap_bytes: values
                .get("SwapPss")
                .or_else(|| values.get("Swap"))
                .copied()
                .unwrap_or(0)
                .saturating_mul(1024),
            pss_available: values.contains_key("Pss"),
        };
    }
    let values = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .map(|value| memory_key_values(&value))
        .unwrap_or_default();
    MemoryUsage {
        rss_bytes: values
            .get("VmRSS")
            .copied()
            .unwrap_or(0)
            .saturating_mul(1024),
        swap_bytes: values
            .get("VmSwap")
            .copied()
            .unwrap_or(0)
            .saturating_mul(1024),
        ..MemoryUsage::default()
    }
}

fn memory_key_values(value: &str) -> HashMap<String, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| {
            Some((
                key.to_owned(),
                value.split_whitespace().next()?.parse().ok()?,
            ))
        })
        .collect()
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
    use super::{
        DiskFile, DiskFileId, MemoryUsage, ProcessIo, ProcessUsage, ResourceSnapshot,
        equals_key_values, parse_process_stat, whitespace_key_values,
    };
    use anyhow::Context;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn parses_proc_stat_with_spaces_in_command() -> anyhow::Result<()> {
        let stat = "42 (application helper) S 7 0 0 0 0 0 0 0 0 0 120 30 0 0 0 0 0 0 99 0 0";
        let process = parse_process_stat(stat).context("valid stat")?;
        assert_eq!(process.parent_pid, 7);
        assert_eq!(process.total_ticks, 150);
        assert_eq!(process.start_ticks, 99);
        assert_eq!(process.major_faults, 0);
        Ok(())
    }

    #[test]
    fn parses_cgroup_cpu_and_io_counters() {
        let cpu = whitespace_key_values("usage_usec 125000\nuser_usec 100000\n");
        assert_eq!(cpu["usage_usec"], 125_000);
        let io = equals_key_values("8:0 rbytes=4096 wbytes=8192 rios=3 wios=4");
        assert_eq!(io["rbytes"], 4096);
        assert_eq!(io["wios"], 4);
    }

    #[test]
    fn totals_process_trees_without_double_counting_shared_roots() {
        let file = |inode, bytes| {
            (
                DiskFileId { device: 1, inode },
                DiskFile {
                    bytes,
                    temporary: false,
                },
            )
        };
        let process = |parent_pid,
                       cpu_percent,
                       memory_bytes,
                       disk_read_bytes,
                       disk_write_bytes,
                       open_files: HashMap<_, _>| ProcessUsage {
            parent_pid,
            cpu_percent,
            memory: MemoryUsage {
                rss_bytes: memory_bytes,
                ..MemoryUsage::default()
            },
            thread_count: 1,
            major_faults: 0,
            gpu_available: false,
            gpu_percent: 0.0,
            gpu_busy_percent: 0.0,
            gpu_memory_resident_bytes: 0,
            gpu_memory_allocated_bytes: 0,
            io: ProcessIo {
                physical_read_bytes: disk_read_bytes,
                physical_write_bytes: disk_write_bytes,
                ..ProcessIo::default()
            },
            referenced_files: open_files.clone(),
            open_files,
            network_sockets: HashSet::new(),
            storage_available: true,
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
            cgroup_members_by_root: HashMap::new(),
            cgroup_path_by_root: HashMap::new(),
            cgroup_usage: HashMap::new(),
            app_disk_by_target: HashMap::new(),
            shared_pids: HashSet::new(),
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
        assert_eq!(usage.storage.referenced_file_permanent_bytes, 3072);
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
                    major_faults: 10,
                    io: Some(ProcessIo {
                        physical_read_bytes: 1_000,
                        physical_write_bytes: 2_000,
                        ..ProcessIo::default()
                    }),
                },
            )]),
            ..Default::default()
        };
        let process = super::ProcessStat {
            parent_pid: 1,
            total_ticks: 500,
            start_ticks: 8,
            major_faults: 20,
            thread_count: 1,
        };
        let current = ProcessIo {
            physical_read_bytes: 4_000,
            physical_write_bytes: 8_000,
            ..ProcessIo::default()
        };
        assert_eq!(sampler.cpu_percent(42, &process, Some(100), 4), 0.0);
        assert_eq!(
            sampler.io_delta(42, &process, current).physical_read_bytes,
            0
        );
        assert_eq!(sampler.major_fault_delta(42, &process), 0);
    }
}
