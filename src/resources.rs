use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::{
    metrics::{finite_nonnegative, rate, rounded},
    model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage},
};

mod energy;
mod gpu;
mod system;

use energy::{BatterySample, EnergyProvider, EnergySampler};
use gpu::{GpuProcessStat, read_gpu_processes};
use system::{
    application_disk_usage, cgroup_members_for_paths, cgroup_paths_for_roots, descendants,
    merge_disk_files, process_cgroup, process_children, read_cgroup_counters, read_cgroup_members,
    read_process_file_sets, read_process_io, read_process_memory, read_processes, read_system_cpu,
    shared_target_pids,
};
#[cfg(test)]
use system::{equals_key_values, parse_process_stat, whitespace_key_values};

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
    rss_available: bool,
    pss_available: bool,
}

#[derive(Debug, Clone, Default)]
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
    files: Arc<ProcessFiles>,
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

const MAX_SAMPLING_WORKERS: usize = 6;

trait ResourceProvider: Debug + EnergyProvider + Send + Sync {
    fn system_cpu(&self) -> (u64, usize);
    fn processes(&self) -> HashMap<u32, ProcessStat>;
    fn process_memory(&self, pid: u32) -> MemoryUsage;
    fn process_io(&self, pid: u32) -> Option<ProcessIo>;
    fn process_files(&self, pid: u32) -> ProcessFiles;
    fn gpu_processes(&self, pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat>;
    fn process_cgroup(&self, pid: u32) -> Option<String>;
    fn cgroup_counters(&self, path: &str) -> Option<CgroupCounters>;
    fn cgroup_members(&self, path: &str) -> HashSet<u32>;
    fn application_disk_usage(&self, target_id: &str) -> DiskBreakdown;
}

#[derive(Debug, Default)]
struct LinuxResourceProvider;

impl EnergyProvider for LinuxResourceProvider {
    fn rapl_zones(&self) -> HashMap<PathBuf, (u64, u64)> {
        energy::read_rapl_zones()
    }

    fn batteries(&self) -> BatterySample {
        energy::read_batteries()
    }
}

impl ResourceProvider for LinuxResourceProvider {
    fn system_cpu(&self) -> (u64, usize) {
        read_system_cpu()
    }

    fn processes(&self) -> HashMap<u32, ProcessStat> {
        read_processes()
    }

    fn process_memory(&self, pid: u32) -> MemoryUsage {
        read_process_memory(pid)
    }

    fn process_io(&self, pid: u32) -> Option<ProcessIo> {
        read_process_io(pid)
    }

    fn process_files(&self, pid: u32) -> ProcessFiles {
        read_process_file_sets(pid)
    }

    fn gpu_processes(&self, pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
        read_gpu_processes(pids)
    }

    fn process_cgroup(&self, pid: u32) -> Option<String> {
        process_cgroup(pid)
    }

    fn cgroup_counters(&self, path: &str) -> Option<CgroupCounters> {
        read_cgroup_counters(path)
    }

    fn cgroup_members(&self, path: &str) -> HashSet<u32> {
        read_cgroup_members(path)
    }

    fn application_disk_usage(&self, target_id: &str) -> DiskBreakdown {
        application_disk_usage(target_id)
    }
}

fn bounded_map<T, R>(items: Vec<T>, operation: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let workers = items.len().min(MAX_SAMPLING_WORKERS);
    if workers <= 1 {
        return items.into_iter().map(operation).collect();
    }
    let queue = Mutex::new(VecDeque::from(items));
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let operation = &operation;
            let queue = &queue;
            let results = &results;
            scope.spawn(move || {
                loop {
                    let item = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop_front();
                    let Some(item) = item else {
                        break;
                    };
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(operation(item));
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
        let mut memory_processes = 0_u64;
        let mut pss_processes = 0_u64;
        let mut gpu_processes = 0_u64;
        let mut network_processes = 0_u64;
        for process in pids.iter().filter_map(|pid| self.processes.get(pid)) {
            usage.add_process(process);
            covered_processes += 1;
            memory_processes += u64::from(process.memory.rss_available);
            pss_processes += u64::from(process.memory.pss_available);
            gpu_processes += u64::from(process.gpu_available);
            network_processes += u64::from(process.files.fd_available);
            merge_disk_files(&mut open_files, &process.files.open);
            merge_disk_files(&mut referenced_files, &process.files.referenced);
            network_sockets.extend(process.files.sockets.iter().copied());
        }
        let process_cpu_percent = usage.compute.cpu_percent;
        let complete_cgroup = !roots.is_empty()
            && cgroup_roots == roots.len()
            && cgroup_paths
                .iter()
                .all(|path| self.cgroup_usage.contains_key(*path));
        if complete_cgroup {
            let mut cgroup = CgroupUsage::default();
            for usage in cgroup_paths
                .iter()
                .filter_map(|path| self.cgroup_usage.get(*path))
            {
                cgroup.cpu_percent += usage.cpu_percent;
                add_counter(&mut cgroup.read_bytes, usage.read_bytes);
                add_counter(&mut cgroup.write_bytes, usage.write_bytes);
                add_counter(&mut cgroup.read_operations, usage.read_operations);
                add_counter(&mut cgroup.write_operations, usage.write_operations);
                add_counter(&mut cgroup.memory_bytes, usage.memory_bytes);
                add_counter(&mut cgroup.swap_bytes, usage.swap_bytes);
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
            let storage = &mut usage.storage;
            add_counter(&mut storage.referenced_file_disk_bytes, file.bytes);
            let classified = if file.temporary {
                &mut storage.referenced_file_temporary_bytes
            } else {
                &mut storage.referenced_file_permanent_bytes
            };
            add_counter(classified, file.bytes);
        }
        usage.measurement.sample_interval_ms = (self.interval_seconds * 1000.0).round() as u64;
        usage.measurement.attribution_method = if complete_cgroup {
            "cgroup".into()
        } else if cgroup_roots > 0 {
            "mixed".into()
        } else {
            "process-tree".into()
        };
        usage.measurement.coverage = if complete_cgroup {
            1.0
        } else if pids.is_empty() {
            0.0
        } else {
            covered_processes as f64 / pids.len() as f64
        };
        usage.measurement.memory_source =
            if covered_processes > 0 && pss_processes == covered_processes {
                "pss".into()
            } else if memory_processes > 0 {
                "rss-fallback".into()
            } else {
                "unavailable".into()
            };
        usage.measurement.gpu_available = gpu_processes > 0;
        usage.measurement.storage_available = complete_cgroup
            || pids
                .iter()
                .filter_map(|pid| self.processes.get(pid))
                .any(|process| process.storage_available);
        usage.network.network_connection_count = network_sockets.len() as u64;
        usage.measurement.network_available = network_processes > 0;
        usage.measurement.network_bytes_available = false;
        usage.measurement.network_connections_available = network_processes > 0;
        usage.measurement.resources_shared = pids.iter().any(|pid| self.shared_pids.contains(pid));
        self.complete(usage, process_cpu_percent)
    }

    fn complete(&self, mut usage: ResourceUsage, energy_cpu_percent: f64) -> ResourceUsage {
        usage.compute.major_faults_per_second = rate(
            usage.compute.major_faults_per_second,
            self.interval_seconds,
            2,
        );
        usage.compute.normalize_cpu(self.logical_cpus);
        usage.storage.normalize_rates(self.interval_seconds);
        usage.energy = self.estimated_energy(energy_cpu_percent, self.total_process_cpu_percent);
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
        let compute = &mut self.compute;
        compute.cpu_percent += process.cpu_percent;
        let memory = &process.memory;
        add_counter(
            &mut compute.memory_bytes,
            if memory.pss_available {
                memory.pss_bytes
            } else {
                memory.rss_bytes
            },
        );
        add_counter(&mut compute.memory_rss_bytes, memory.rss_bytes);
        add_counter(&mut compute.memory_pss_bytes, memory.pss_bytes);
        add_counter(&mut compute.memory_private_bytes, memory.private_bytes);
        add_counter(&mut compute.memory_swap_bytes, memory.swap_bytes);
        add_counter(&mut compute.process_count, 1);
        add_counter(&mut compute.thread_count, process.thread_count);
        compute.major_faults_per_second += process.major_faults as f64;
        compute.gpu_percent += process.gpu_percent;
        compute.gpu_busy_percent = compute.gpu_busy_percent.max(process.gpu_busy_percent);
        add_counter(
            &mut compute.gpu_memory_resident_bytes,
            process.gpu_memory_resident_bytes,
        );
        add_counter(
            &mut compute.gpu_memory_allocated_bytes,
            process.gpu_memory_allocated_bytes,
        );
        add_counter(
            &mut compute.gpu_memory_bytes,
            match process.gpu_memory_resident_bytes {
                0 => process.gpu_memory_allocated_bytes,
                resident => resident,
            },
        );

        let storage = &mut self.storage;
        add_counter(&mut storage.disk_read_bytes, process.io.physical_read_bytes);
        add_counter(
            &mut storage.disk_write_bytes,
            process.io.physical_write_bytes,
        );
        add_counter(
            &mut storage.logical_read_bytes,
            process.io.logical_read_bytes,
        );
        add_counter(
            &mut storage.logical_write_bytes,
            process.io.logical_write_bytes,
        );
        add_counter(&mut storage.read_operations, process.io.read_operations);
        add_counter(&mut storage.write_operations, process.io.write_operations);
        add_counter(
            &mut storage.cancelled_write_bytes,
            process.io.cancelled_write_bytes,
        );
    }
}

fn add_counter(counter: &mut u64, value: u64) {
    *counter = counter.saturating_add(value);
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

#[derive(Debug)]
pub struct ResourceSampler {
    provider: Arc<dyn ResourceProvider>,
    previous_processes: HashMap<u32, PreviousProcess>,
    previous_gpu_engines: HashMap<(u32, u64, String), u64>,
    previous_system_ticks: Option<u64>,
    previous_cgroups: HashMap<String, CgroupCounters>,
    previous_sample: Option<Instant>,
    open_files: OpenFileCache,
    app_disk: AppDiskCache,
    energy: EnergySampler,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self {
            provider: Arc::new(LinuxResourceProvider),
            previous_processes: HashMap::new(),
            previous_gpu_engines: HashMap::new(),
            previous_system_ticks: None,
            previous_cgroups: HashMap::new(),
            previous_sample: None,
            open_files: OpenFileCache::default(),
            app_disk: AppDiskCache::default(),
            energy: EnergySampler::default(),
        }
    }
}

#[derive(Debug, Default)]
struct OpenFileCache {
    samples: HashMap<u32, Arc<ProcessFiles>>,
    next_refresh: InstantSlot,
}

#[derive(Debug, Clone, Default)]
struct ProcessFiles {
    open: HashMap<DiskFileId, DiskFile>,
    referenced: HashMap<DiskFileId, DiskFile>,
    sockets: HashSet<u64>,
    fd_available: bool,
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
    #[cfg(test)]
    fn with_provider(provider: Arc<dyn ResourceProvider>) -> Self {
        Self {
            provider,
            ..Self::default()
        }
    }

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
        let provider = Arc::clone(&self.provider);
        let (system_ticks, logical_cpus) = provider.system_cpu();
        let system_delta = self
            .previous_system_ticks
            .map(|previous| system_ticks.saturating_sub(previous))
            .filter(|delta| *delta > 0);
        let current = provider.processes();
        let process_children = process_children(&current);
        let active_roots = active_targets
            .values()
            .flatten()
            .copied()
            .filter(|pid| *pid > 0)
            .collect::<HashSet<_>>();
        let cgroup_path_by_root = cgroup_paths_for_roots(provider.as_ref(), &active_roots);
        let cgroup_members_by_root =
            cgroup_members_for_paths(provider.as_ref(), &cgroup_path_by_root);
        let current_cgroups = cgroup_path_by_root
            .values()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|path| Some((path.clone(), provider.cgroup_counters(path)?)))
            .collect::<HashMap<_, _>>();
        let cgroup_usage = self.cgroup_usage(&current_cgroups, interval_seconds);
        let mut active_processes = descendants(active_roots.iter().copied(), &process_children);
        for members in cgroup_members_by_root.values() {
            active_processes.extend(members);
        }
        let current_gpu = provider.gpu_processes(&active_processes);
        let mut current_open_files =
            self.open_files
                .read(provider.as_ref(), &active_processes, now);
        let sampled_details = bounded_map(active_processes.iter().copied().collect(), |pid| {
            (pid, provider.process_memory(pid), provider.process_io(pid))
        })
        .into_iter()
        .map(|(pid, memory, io)| (pid, (memory, io)))
        .collect::<HashMap<_, _>>();
        let energy = self.energy.sample(interval_seconds, provider.as_ref());
        let app_disk_by_target = self
            .app_disk
            .read(provider.as_ref(), active_targets.keys(), now);
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
            let cpu_percent = self.cpu_percent(pid, process, system_delta, logical_cpus);
            let (memory, sampled_io) = sampled_details.get(&pid).copied().unwrap_or_default();
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
                    files,
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
    fn read(
        &mut self,
        provider: &dyn ResourceProvider,
        pids: &HashSet<u32>,
        now: Instant,
    ) -> HashMap<u32, Arc<ProcessFiles>> {
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples.retain(|pid, _| pids.contains(pid));
        let requested = if refresh {
            pids.iter().copied().collect()
        } else {
            pids.iter()
                .filter(|pid| !self.samples.contains_key(pid))
                .copied()
                .collect()
        };
        let sampled = bounded_map(requested, |pid| {
            (pid, Arc::new(provider.process_files(pid)))
        });
        if refresh {
            self.samples = sampled.into_iter().collect();
            self.next_refresh.0 = Some(now + std::time::Duration::from_secs(10));
        } else {
            self.samples.extend(sampled);
        }
        self.samples.clone()
    }
}

impl AppDiskCache {
    fn read<'a>(
        &mut self,
        provider: &dyn ResourceProvider,
        targets: impl IntoIterator<Item = &'a String>,
        now: Instant,
    ) -> HashMap<String, DiskBreakdown> {
        let targets = targets.into_iter().cloned().collect::<HashSet<_>>();
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples.retain(|target, _| targets.contains(target));
        let requested = if refresh {
            targets.into_iter().collect()
        } else {
            targets
                .into_iter()
                .filter(|target| !self.samples.contains_key(target))
                .collect()
        };
        let sampled = bounded_map(requested, |target| {
            let usage = provider.application_disk_usage(&target);
            (target, usage)
        });
        if refresh {
            self.samples = sampled.into_iter().collect();
            self.next_refresh.0 = Some(now + std::time::Duration::from_secs(30));
        } else {
            self.samples.extend(sampled);
        }
        self.samples.clone()
    }
}

impl ResourceSnapshot {
    fn insert(&mut self, pid: u32, process: ProcessUsage) {
        self.total_process_cpu_percent += process.cpu_percent;
        self.children
            .entry(process.parent_pid)
            .or_default()
            .push(pid);
        self.processes.insert(pid, process);
    }
}

#[cfg(test)]
mod tests;
