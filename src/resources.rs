use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use crate::{
    metrics::{finite_nonnegative, rate, rounded},
    model::{ComputeUsage, EnergyUsage, NetworkUsage, ResourceUsage, StorageUsage},
};

mod disk;
mod energy;
mod gpu;
mod network;
mod system;

use disk::AppDiskCache;
use energy::{BatterySample, EnergyProvider, EnergySampler};
use gpu::{GpuProcessStat, read_gpu_processes};
use network::{NetworkCounters, read_network_counters};
#[cfg(test)]
use system::parse_process_stat;
pub(crate) use system::process_cgroup;
use system::{
    application_disk_usage, cgroup_members_for_paths, cgroup_paths_for_roots, descendants,
    merge_disk_files, process_children, read_cgroup_counters, read_cgroup_members,
    read_process_file_sets, read_process_io, read_process_memory, read_process_sockets,
    read_processes, read_system_cpu, shared_target_pids,
};

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
    gpu_engine_percent: HashMap<String, f64>,
    gpu_memory_resident_bytes: u64,
    gpu_memory_allocated_bytes: u64,
    io: ProcessIo,
    files: Arc<ProcessFiles>,
    sockets: Option<HashSet<u64>>,
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

const MEMORY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const OPEN_FILE_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const APP_DISK_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

trait ResourceProvider: Debug + EnergyProvider + Send + Sync {
    fn system_cpu(&self) -> (u64, usize);
    fn processes(&self) -> HashMap<u32, ProcessStat>;
    fn process_memory(&self, pid: u32) -> MemoryUsage;
    fn process_io(&self, pid: u32) -> Option<ProcessIo>;
    fn process_files(&self, pid: u32) -> ProcessFiles;
    fn process_sockets(&self, pid: u32) -> Option<HashSet<u64>>;
    fn network_counters(&self, inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>>;
    fn gpu_processes(&self, pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat>;
    fn process_cgroup(&self, pid: u32) -> Option<String>;
    fn cgroup_counters(&self, path: &str) -> Option<CgroupCounters>;
    fn cgroup_members(&self, path: &str) -> HashSet<u32>;
    fn application_disk_usage(&self, target_id: &str) -> Option<DiskBreakdown>;
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

    fn process_sockets(&self, pid: u32) -> Option<HashSet<u64>> {
        read_process_sockets(pid)
    }

    fn network_counters(&self, inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>> {
        read_network_counters(inodes)
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

    fn application_disk_usage(&self, target_id: &str) -> Option<DiskBreakdown> {
        application_disk_usage(target_id)
    }
}

#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    processes: HashMap<u32, ProcessUsage>,
    children: HashMap<u32, Vec<u32>>,
    cgroup_members_by_root: HashMap<u32, HashSet<u32>>,
    cgroup_path_by_root: HashMap<u32, String>,
    cgroup_usage: HashMap<String, CgroupUsage>,
    app_disk_by_target: HashMap<String, DiskBreakdown>,
    network_deltas: HashMap<u64, NetworkCounters>,
    network_counters_available: bool,
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
            network_deltas: HashMap::new(),
            network_counters_available: false,
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

struct SampledNetwork {
    sockets_by_pid: HashMap<u32, HashSet<u64>>,
    current: HashMap<u64, NetworkCounters>,
    deltas: HashMap<u64, NetworkCounters>,
    available: bool,
}

struct ResourceAttribution {
    roots: HashSet<u32>,
    pids: HashSet<u32>,
    cgroup_paths: HashSet<String>,
    cgroup_roots: usize,
    cgroups_cover_process_trees: bool,
}

#[derive(Default)]
struct ProcessAggregation {
    usage: ResourceUsage,
    open_files: HashMap<DiskFileId, DiskFile>,
    referenced_files: HashMap<DiskFileId, DiskFile>,
    network_sockets: HashSet<u64>,
    covered_processes: u64,
    memory_processes: u64,
    pss_processes: u64,
    gpu_processes: u64,
    gpu_engine_percent: HashMap<String, f64>,
    network_processes: u64,
    storage_processes: u64,
    file_processes: u64,
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
        let attribution = self.resource_attribution(roots);
        let mut aggregate = self.aggregate_processes(&attribution.pids);
        let process_cpu_percent = aggregate.usage.compute.cpu_percent;
        let complete_cgroup = self.has_complete_cgroup_attribution(&attribution);
        if complete_cgroup {
            self.apply_cgroup_usage(&mut aggregate.usage, &attribution.cgroup_paths);
        }
        Self::apply_file_storage(&mut aggregate);
        self.apply_measurement(&mut aggregate, &attribution, complete_cgroup);
        self.complete(aggregate.usage, process_cpu_percent)
    }

    fn resource_attribution(&self, roots: impl IntoIterator<Item = u32>) -> ResourceAttribution {
        let roots = roots
            .into_iter()
            .filter(|pid| *pid > 0)
            .collect::<HashSet<_>>();
        let mut attribution = ResourceAttribution {
            roots: roots.clone(),
            pids: HashSet::new(),
            cgroup_paths: HashSet::new(),
            cgroup_roots: 0,
            cgroups_cover_process_trees: true,
        };
        for root in roots {
            self.attribute_root(&mut attribution, root);
        }
        attribution
    }

    fn attribute_root(&self, attribution: &mut ResourceAttribution, root: u32) {
        // A descendant can move into a sibling scope after it is spawned (terminal
        // emulators commonly do this for each surface). Keep process-tree members in
        // the attribution even when the application root has a specific cgroup.
        let tree = descendants([root], &self.children);
        attribution.pids.extend(&tree);
        let Some(members) = self.cgroup_members_by_root.get(&root) else {
            attribution.cgroups_cover_process_trees = false;
            return;
        };
        attribution.cgroups_cover_process_trees &= tree.is_subset(members);
        attribution.pids.extend(members);
        if let Some(path) = self.cgroup_path_by_root.get(&root) {
            attribution.cgroup_paths.insert(path.clone());
        }
        attribution.cgroup_roots += 1;
    }

    fn aggregate_processes(&self, pids: &HashSet<u32>) -> ProcessAggregation {
        let mut aggregate = ProcessAggregation::default();
        for process in pids.iter().filter_map(|pid| self.processes.get(pid)) {
            aggregate.usage.add_process(process);
            aggregate.covered_processes += 1;
            aggregate.memory_processes += u64::from(process.memory.rss_available);
            aggregate.pss_processes += u64::from(process.memory.pss_available);
            aggregate.gpu_processes += u64::from(process.gpu_available);
            for (engine, percent) in &process.gpu_engine_percent {
                *aggregate
                    .gpu_engine_percent
                    .entry(engine.clone())
                    .or_default() += percent;
            }
            aggregate.network_processes += u64::from(process.sockets.is_some());
            aggregate.storage_processes += u64::from(process.storage_available);
            if process.files.fd_available {
                aggregate.file_processes += 1;
                merge_disk_files(&mut aggregate.open_files, &process.files.open);
                merge_disk_files(&mut aggregate.referenced_files, &process.files.referenced);
            }
            aggregate
                .network_sockets
                .extend(process.sockets.iter().flatten().copied());
        }
        aggregate.usage.compute.gpu_busy_percent = aggregate
            .gpu_engine_percent
            .values()
            .copied()
            .fold(0.0, f64::max);
        aggregate
    }

    fn has_complete_cgroup_attribution(&self, attribution: &ResourceAttribution) -> bool {
        !attribution.roots.is_empty()
            && attribution.cgroup_roots == attribution.roots.len()
            && attribution.cgroups_cover_process_trees
            && attribution
                .cgroup_paths
                .iter()
                .all(|path| self.cgroup_usage.contains_key(path))
    }

    fn apply_cgroup_usage(&self, usage: &mut ResourceUsage, paths: &HashSet<String>) {
        let mut cgroup = CgroupUsage::default();
        for current in paths.iter().filter_map(|path| self.cgroup_usage.get(path)) {
            cgroup.cpu_percent += current.cpu_percent;
            add_counter(&mut cgroup.read_bytes, current.read_bytes);
            add_counter(&mut cgroup.write_bytes, current.write_bytes);
            add_counter(&mut cgroup.read_operations, current.read_operations);
            add_counter(&mut cgroup.write_operations, current.write_operations);
            add_counter(&mut cgroup.memory_bytes, current.memory_bytes);
            add_counter(&mut cgroup.swap_bytes, current.swap_bytes);
        }
        usage.compute.cpu_percent = cgroup.cpu_percent;
        usage.compute.memory_cgroup_bytes = cgroup.memory_bytes;
        usage.storage.disk_read_bytes = cgroup.read_bytes;
        usage.storage.disk_write_bytes = cgroup.write_bytes;
        usage.storage.read_operations = cgroup.read_operations;
        usage.storage.write_operations = cgroup.write_operations;
    }

    fn apply_file_storage(aggregate: &mut ProcessAggregation) {
        aggregate.usage.storage.open_file_disk_bytes =
            aggregate.open_files.values().map(|file| file.bytes).sum();
        for file in aggregate.referenced_files.values() {
            let storage = &mut aggregate.usage.storage;
            add_counter(&mut storage.referenced_file_disk_bytes, file.bytes);
            let classified = if file.temporary {
                &mut storage.referenced_file_temporary_bytes
            } else {
                &mut storage.referenced_file_permanent_bytes
            };
            add_counter(classified, file.bytes);
        }
    }

    fn apply_measurement(
        &self,
        aggregate: &mut ProcessAggregation,
        attribution: &ResourceAttribution,
        complete_cgroup: bool,
    ) {
        let network_bytes_available =
            self.apply_network(&mut aggregate.usage.network, &aggregate.network_sockets);
        let coverage = measurement_coverage(complete_cgroup, aggregate, attribution);
        let memory_source = memory_source(aggregate);
        let measurement = &mut aggregate.usage.measurement;
        measurement.sample_interval_ms = (self.interval_seconds * 1000.0).round() as u64;
        measurement.attribution_method = attribution_method(complete_cgroup, attribution);
        measurement.coverage = coverage;
        measurement.memory_source = memory_source;
        measurement.gpu_available = aggregate.gpu_processes > 0;
        measurement.storage_available = complete_cgroup || aggregate.storage_processes > 0;
        measurement.referenced_files_available = aggregate.file_processes > 0;
        measurement.network_available = aggregate.network_processes > 0;
        measurement.network_bytes_available = network_bytes_available;
        measurement.network_connections_available = aggregate.network_processes > 0;
        measurement.resources_shared = attribution
            .pids
            .iter()
            .any(|pid| self.shared_pids.contains(pid));
    }

    fn apply_network(&self, usage: &mut NetworkUsage, sockets: &HashSet<u64>) -> bool {
        usage.network_connection_count = sockets.len() as u64;
        let counters = sockets
            .iter()
            .filter_map(|inode| self.network_deltas.get(inode));
        let mut measured_connections = 0;
        for current in counters {
            add_counter(&mut usage.network_receive_bytes, current.received_bytes);
            add_counter(&mut usage.network_transmit_bytes, current.transmitted_bytes);
            measured_connections += 1;
        }
        self.network_counters_available && measured_connections > 0
    }

    fn complete(&self, mut usage: ResourceUsage, energy_cpu_percent: f64) -> ResourceUsage {
        usage.compute.major_faults_per_second = rate(
            usage.compute.major_faults_per_second,
            self.interval_seconds,
            2,
        );
        usage.compute.normalize_cpu(self.logical_cpus);
        usage.storage.normalize_rates(self.interval_seconds);
        usage.network.normalize_rates(self.interval_seconds);
        usage.energy = self.estimated_energy(energy_cpu_percent, self.total_process_cpu_percent);
        usage
    }

    fn estimated_energy(&self, activity: f64, total: f64) -> EnergyUsage {
        let (attributed_fraction, energy_mwh, confidence) =
            self.energy_attribution(activity, total);
        let power_watts = rate(energy_mwh * 3.6, self.interval_seconds, 3);
        EnergyUsage {
            energy_mwh,
            battery_percent: rate(energy_mwh * 100.0, self.battery_full_mwh, 6),
            power_watts,
            estimated_app_power_watts: power_watts,
            system_power_watts: rate(self.system_energy_mwh * 3.6, self.interval_seconds, 3),
            battery_percent_per_hour: rate(power_watts * 100_000.0, self.battery_full_mwh, 4),
            attributed_fraction,
            energy_source: self.energy_source.clone(),
            energy_confidence: confidence.into(),
        }
    }

    fn energy_attribution(&self, activity: f64, total: f64) -> (f64, f64, &'static str) {
        if self.energy_source != "rapl" {
            return (0.0, 0.0, "system-only");
        }
        let share = if total > 0.0 {
            (activity / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (
            rounded(share, 4),
            rounded(self.system_energy_mwh * share, 4),
            "low",
        )
    }

    pub fn interval_seconds(&self) -> f64 {
        self.interval_seconds
    }
}

fn attribution_method(complete_cgroup: bool, attribution: &ResourceAttribution) -> String {
    match (complete_cgroup, attribution.cgroup_roots > 0) {
        (true, _) => "cgroup",
        (false, true) => "mixed",
        (false, false) => "process-tree",
    }
    .into()
}

fn measurement_coverage(
    complete_cgroup: bool,
    aggregate: &ProcessAggregation,
    attribution: &ResourceAttribution,
) -> f64 {
    match (complete_cgroup, attribution.pids.len()) {
        (true, _) => 1.0,
        (false, 0) => 0.0,
        (false, process_count) => aggregate.covered_processes as f64 / process_count as f64,
    }
}

fn memory_source(aggregate: &ProcessAggregation) -> String {
    match (
        aggregate.covered_processes,
        aggregate.pss_processes,
        aggregate.memory_processes,
    ) {
        (covered, pss, _) if covered > 0 && pss == covered => "pss",
        (_, _, memory) if memory > 0 => "rss-fallback",
        _ => "unavailable",
    }
    .into()
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
        compute.gpu_percent += process.gpu_engine_percent.values().sum::<f64>();
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

impl crate::model::NetworkUsage {
    fn normalize_rates(&mut self, seconds: f64) {
        self.network_receive_bytes_per_second = rate(self.network_receive_bytes as f64, seconds, 1);
        self.network_transmit_bytes_per_second =
            rate(self.network_transmit_bytes as f64, seconds, 1);
    }
}

#[derive(Debug)]
pub struct ResourceSampler {
    provider: Arc<dyn ResourceProvider>,
    previous_processes: HashMap<u32, PreviousProcess>,
    previous_gpu_engines: HashMap<(u32, u64, String), u64>,
    previous_system_ticks: Option<u64>,
    previous_cgroups: HashMap<String, CgroupCounters>,
    previous_network_counters: HashMap<u64, NetworkCounters>,
    previous_sockets_by_pid: HashMap<u32, HashSet<u64>>,
    previous_network_available: bool,
    previous_sample: Option<Instant>,
    memory: MemoryCache,
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
            previous_network_counters: HashMap::new(),
            previous_sockets_by_pid: HashMap::new(),
            previous_network_available: false,
            previous_sample: None,
            memory: MemoryCache::default(),
            open_files: OpenFileCache::default(),
            app_disk: AppDiskCache::default(),
            energy: EnergySampler::default(),
        }
    }
}

type ProcessIdentity = (u32, u64);

#[derive(Debug, Default)]
struct MemoryCache {
    samples: HashMap<ProcessIdentity, MemoryUsage>,
    next_refresh: InstantSlot,
}

#[derive(Debug, Default)]
struct OpenFileCache {
    samples: HashMap<ProcessIdentity, Arc<ProcessFiles>>,
    next_refresh: InstantSlot,
}

#[derive(Debug, Clone, Default)]
struct ProcessFiles {
    open: HashMap<DiskFileId, DiskFile>,
    referenced: HashMap<DiskFileId, DiskFile>,
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
        active_processes.retain(|pid| current.contains_key(pid));
        let identities = active_processes
            .iter()
            .map(|&pid| (pid, current[&pid].start_ticks))
            .collect::<HashSet<_>>();
        self.previous_sockets_by_pid.retain(|pid, _| {
            current
                .get(pid)
                .zip(self.previous_processes.get(pid))
                .is_some_and(|(current, previous)| current.start_ticks == previous.start_ticks)
        });
        let current_gpu = provider.gpu_processes(&active_processes);
        self.open_files.refresh(provider.as_ref(), &identities, now);
        let mut network = self.sample_network(provider.as_ref(), &active_processes);
        self.memory.refresh(provider.as_ref(), &identities, now);
        let sampled_io = active_processes
            .iter()
            .copied()
            .map(|pid| (pid, provider.process_io(pid)))
            .collect::<HashMap<_, _>>();
        let energy = self.energy.sample(interval_seconds, provider.as_ref());
        let app_disk_by_target = self
            .app_disk
            .read(&provider, active_targets.keys(), now);
        let shared_pids =
            shared_target_pids(active_targets, &process_children, &cgroup_members_by_root);
        let mut snapshot = ResourceSnapshot {
            cgroup_members_by_root,
            cgroup_path_by_root,
            cgroup_usage,
            app_disk_by_target,
            network_deltas: network.deltas,
            network_counters_available: network.available,
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
            let identity = (pid, process.start_ticks);
            let memory = self
                .memory
                .samples
                .get(&identity)
                .copied()
                .unwrap_or_default();
            let sampled_io = sampled_io.get(&pid).copied().flatten();
            let io = self.io_delta(pid, process, sampled_io.unwrap_or_default());
            if let Some(value) = sampled_io {
                current_io.insert(pid, value);
            }
            let major_faults = self.major_fault_delta(pid, process);
            let gpu = current_gpu.get(&pid);
            let gpu_engine_percent = self.gpu_percent(
                pid,
                process.start_ticks,
                gpu,
                interval_seconds,
                &mut next_gpu_engines,
            );
            let files = self
                .open_files
                .samples
                .get(&identity)
                .cloned()
                .unwrap_or_default();
            snapshot.insert(
                pid,
                ProcessUsage {
                    parent_pid: process.parent_pid,
                    cpu_percent,
                    memory,
                    thread_count: process.thread_count,
                    major_faults,
                    gpu_available: gpu.is_some(),
                    gpu_engine_percent,
                    gpu_memory_resident_bytes: gpu.map_or(0, |gpu| gpu.resident_memory_bytes),
                    gpu_memory_allocated_bytes: gpu.map_or(0, |gpu| gpu.allocated_memory_bytes),
                    io,
                    files,
                    sockets: network.sockets_by_pid.remove(&pid),
                    storage_available: sampled_io.is_some(),
                },
            );
        }
        self.remember(
            (current, current_io),
            current_cgroups,
            network.current,
            next_gpu_engines,
            system_ticks,
            now,
        );
        snapshot
    }

    fn sample_network(
        &mut self,
        provider: &dyn ResourceProvider,
        active_processes: &HashSet<u32>,
    ) -> SampledNetwork {
        // Socket discovery is lightweight and must not share the file-footprint TTL.
        let sockets_by_pid = active_processes
            .iter()
            .filter_map(|&pid| Some((pid, provider.process_sockets(pid)?)))
            .collect::<HashMap<_, _>>();
        let sockets = sockets_by_pid
            .values()
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        let previous_sockets = self
            .previous_sockets_by_pid
            .values()
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        let newly_opened = sockets_by_pid
            .iter()
            .filter(|(pid, _)| self.previous_sockets_by_pid.contains_key(pid))
            .flat_map(|(_, sockets)| sockets.iter().copied())
            .filter(|inode| !previous_sockets.contains(inode))
            .collect::<HashSet<_>>();
        let sampled = provider.network_counters(&sockets);
        let available = sampled.is_some();
        let current = sampled.unwrap_or_default();
        let deltas = current
            .iter()
            .map(|(&inode, &counters)| {
                let previous = self
                    .previous_network_counters
                    .get(&inode)
                    .copied()
                    .unwrap_or_else(|| {
                        // Baseline existing sockets on startup, newly attributed processes,
                        // or recovery. Include initial bytes only for newly opened sockets
                        // in processes whose descriptors we observed last interval.
                        if self.previous_network_available && newly_opened.contains(&inode) {
                            NetworkCounters::default()
                        } else {
                            counters
                        }
                    });
                (
                    inode,
                    NetworkCounters {
                        received_bytes: counters
                            .received_bytes
                            .saturating_sub(previous.received_bytes),
                        transmitted_bytes: counters
                            .transmitted_bytes
                            .saturating_sub(previous.transmitted_bytes),
                    },
                )
            })
            .collect();
        self.previous_sockets_by_pid = sockets_by_pid.clone();
        self.previous_network_available = available;
        SampledNetwork {
            sockets_by_pid,
            current,
            deltas,
            available,
        }
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
    ) -> HashMap<String, f64> {
        let mut engines = HashMap::<String, f64>::new();
        let Some(gpu) = gpu else {
            return engines;
        };
        for (client_engine, &nanoseconds) in &gpu.engine_nanoseconds {
            let key = (pid, start_ticks, client_engine.clone());
            let previous = self.previous_gpu_engines.get(&key).copied();
            // Seed the baseline even on the first (zero-duration) sample.
            next.insert(key, nanoseconds);
            let elapsed = previous.map_or(0, |value| nanoseconds.saturating_sub(value));
            let percent = if seconds > 0.0 {
                finite_nonnegative(elapsed as f64 / (seconds * 1_000_000_000.0) * 100.0)
            } else {
                0.0
            };
            *engines.entry(gpu::engine_scope(client_engine)).or_default() += percent;
        }
        engines
    }

    fn remember(
        &mut self,
        processes: (HashMap<u32, ProcessStat>, HashMap<u32, ProcessIo>),
        current_cgroups: HashMap<String, CgroupCounters>,
        current_network_counters: HashMap<u64, NetworkCounters>,
        gpu_engines: HashMap<(u32, u64, String), u64>,
        system_ticks: u64,
        now: Instant,
    ) {
        let (current, current_io) = processes;
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
        self.previous_network_counters = current_network_counters;
        self.previous_system_ticks = Some(system_ticks);
        self.previous_sample = Some(now);
    }
}

impl MemoryCache {
    fn refresh(
        &mut self,
        provider: &dyn ResourceProvider,
        identities: &HashSet<ProcessIdentity>,
        now: Instant,
    ) {
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples
            .retain(|identity, _| identities.contains(identity));
        if refresh {
            self.samples = identities
                .iter()
                .copied()
                .map(|identity| (identity, provider.process_memory(identity.0)))
                .collect();
            self.next_refresh.0 = Some(now + MEMORY_REFRESH_INTERVAL);
        } else {
            let missing = identities
                .iter()
                .filter(|identity| !self.samples.contains_key(identity))
                .copied()
                .collect::<Vec<_>>();
            for identity in missing {
                self.samples
                    .insert(identity, provider.process_memory(identity.0));
            }
        }
    }
}

impl OpenFileCache {
    fn refresh(
        &mut self,
        provider: &dyn ResourceProvider,
        identities: &HashSet<ProcessIdentity>,
        now: Instant,
    ) {
        let refresh = self.next_refresh.0.is_none_or(|deadline| now >= deadline);
        self.samples
            .retain(|identity, _| identities.contains(identity));
        let requested: Vec<ProcessIdentity> = if refresh {
            identities.iter().copied().collect()
        } else {
            identities
                .iter()
                .filter(|identity| !self.samples.contains_key(identity))
                .copied()
                .collect()
        };
        let sampled = requested
            .into_iter()
            .map(|identity| (identity, Arc::new(provider.process_files(identity.0))))
            .collect::<Vec<_>>();
        if refresh {
            self.samples = sampled.into_iter().collect();
            self.next_refresh.0 = Some(now + OPEN_FILE_REFRESH_INTERVAL);
        } else {
            self.samples.extend(sampled);
        }
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
mod gpu_usage_tests;
#[cfg(test)]
mod network_tests;
#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod test_provider;
#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod tests;
