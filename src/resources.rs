use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use shelllist_daemon_tokio::ResumeDetector;

use crate::{
    metrics::{add_counter, available_label, finite_nonnegative, rate, rounded},
    model::{EnergyUsage, NetworkUsage, ResourceUsage},
    process::descendants,
};

mod discovery;
mod disk;
mod energy;
mod gpu;
mod network;
mod provider;
mod system;

use discovery::ProcessSample;
use disk::AppDiskCache;
use energy::EnergySampler;
use provider::{
    CgroupCounters, DiskBreakdown, DiskFile, DiskFileId, IoCounters, MemoryUsage, NetworkCounters,
    ProcessFiles, ProcessIo, ProcessStat, ResourceProvider,
};
use system::{LinuxResourceProvider, merge_disk_files};

#[derive(Debug, Clone, Copy)]
struct PreviousProcess {
    total_ticks: u64,
    start_ticks: u64,
    major_faults: u64,
    io: Option<ProcessIo>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CgroupUsage {
    cpu_percent: Option<f64>,
    io: Option<IoCounters>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct ProcessUsage {
    cpu_percent: f64,
    memory: MemoryUsage,
    thread_count: u64,
    major_faults: u64,
    gpu: gpu::Usage,
    io: ProcessIo,
    files: Option<Arc<ProcessFiles>>,
    sockets: Option<Arc<HashSet<u64>>>,
    storage_available: bool,
}

const MEMORY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const OPEN_FILE_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Default)]
pub struct ResourceSnapshot {
    target_roots: HashMap<String, Vec<u32>>,
    target_owners: HashMap<u32, Arc<str>>,
    processes: HashMap<u32, ProcessUsage>,
    children: HashMap<u32, Vec<u32>>,
    cgroup_members_by_path: HashMap<String, HashSet<u32>>,
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

struct ResourceAttribution<'a> {
    root_count: usize,
    pids: HashSet<u32>,
    cgroup_paths: HashSet<&'a str>,
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
    network_processes: u64,
    storage_processes: u64,
    file_processes: u64,
}

impl ResourceSnapshot {
    pub(crate) fn runtime_revision(&self) -> u64 {
        crate::metrics::unordered_revision(self.target_roots.keys())
    }

    pub(crate) fn target_roots(&self) -> &HashMap<String, Vec<u32>> {
        &self.target_roots
    }

    pub(crate) fn usage_for_application(
        &self,
        target_id: &str,
        windows: impl IntoIterator<Item = u32>,
    ) -> ResourceUsage {
        self.usage_for_target(
            target_id,
            windows.into_iter().chain(
                self.target_roots
                    .get(target_id)
                    .into_iter()
                    .flatten()
                    .copied(),
            ),
        )
    }

    pub fn usage_for_target(
        &self,
        target_id: &str,
        roots: impl IntoIterator<Item = u32>,
    ) -> ResourceUsage {
        let mut usage = self.usage_for_roots_filtered(roots, Some(target_id));
        if let Some(disk) = self.app_disk_by_target.get(target_id) {
            usage.storage.disk_space_total_bytes = disk.total_bytes;
            usage.storage.disk_space_temporary_bytes = disk.temporary_bytes;
            usage.storage.disk_space_permanent_bytes = disk.permanent_bytes;
            usage.measurement.disk_space_scope = "identified-app-directories".into();
        } else {
            usage.measurement.disk_space_scope = "unavailable".into();
        }
        usage.with_availability()
    }

    #[cfg(test)]
    fn usage_for_roots(&self, roots: impl IntoIterator<Item = u32>) -> ResourceUsage {
        self.usage_for_roots_filtered(roots, None)
            .with_availability()
    }

    fn usage_for_roots_filtered(
        &self,
        roots: impl IntoIterator<Item = u32>,
        target: Option<&str>,
    ) -> ResourceUsage {
        let mut attribution = self.resource_attribution(roots);
        if let Some(target) = target {
            let before = attribution.pids.len();
            attribution.pids.retain(|pid| {
                self.target_owners
                    .get(pid)
                    .is_none_or(|owner| owner.as_ref() == target)
            });
            // Cgroup totals include nested subgroups: when another application's
            // members were excluded, use process counters rather than double charge.
            attribution.cgroups_cover_process_trees &= attribution.pids.len() == before;
        }
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

    fn resource_attribution(
        &self,
        roots: impl IntoIterator<Item = u32>,
    ) -> ResourceAttribution<'_> {
        let roots = roots
            .into_iter()
            .filter(|pid| *pid > 0)
            .collect::<HashSet<_>>();
        let mut attribution = ResourceAttribution {
            root_count: roots.len(),
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

    fn attribute_root<'a>(&'a self, attribution: &mut ResourceAttribution<'a>, root: u32) {
        // A descendant can move into a sibling scope after it is spawned (terminal
        // emulators commonly do this for each surface). Keep process-tree members in
        // the attribution even when the application root has a specific cgroup.
        let tree = descendants([root], &self.children);
        attribution.pids.extend(&tree);
        let group = self
            .cgroup_path_by_root
            .get(&root)
            .and_then(|path| Some((path, self.cgroup_members_by_path.get(path)?)));
        let Some((path, members)) = group else {
            attribution.cgroups_cover_process_trees = false;
            return;
        };
        attribution.cgroups_cover_process_trees &= tree.is_subset(members);
        attribution.pids.extend(members);
        attribution.cgroup_paths.insert(path.as_str());
        attribution.cgroup_roots += 1;
    }

    fn shared_target_pids(&self, targets: &HashMap<String, Vec<u32>>) -> HashSet<u32> {
        let mut seen = HashSet::new();
        targets
            .values()
            .flat_map(|roots| {
                let mut pids = descendants(roots.iter().copied(), &self.children);
                pids.extend(
                    roots
                        .iter()
                        .filter_map(|root| self.cgroup_path_by_root.get(root))
                        .filter_map(|path| self.cgroup_members_by_path.get(path))
                        .flatten(),
                );
                pids
            })
            .filter(|pid| !seen.insert(*pid))
            .collect()
    }

    fn aggregate_processes(&self, pids: &HashSet<u32>) -> ProcessAggregation {
        let mut aggregate = ProcessAggregation::default();
        let mut gpu_engines = HashMap::<&str, f64>::new();
        for process in pids.iter().filter_map(|pid| self.processes.get(pid)) {
            process.add_to(&mut aggregate.usage);
            aggregate.covered_processes += 1;
            aggregate.memory_processes += u64::from(process.memory.rss_available);
            aggregate.pss_processes += u64::from(process.memory.pss_available);
            aggregate.gpu_processes += u64::from(process.gpu.available);
            for (engine, percent) in &process.gpu.engines {
                *gpu_engines.entry(engine).or_default() += percent;
            }
            aggregate.network_processes += u64::from(process.sockets.is_some());
            aggregate.storage_processes += u64::from(process.storage_available);
            if let Some(files) = process.files.as_ref().filter(|files| files.fd_available) {
                aggregate.file_processes += 1;
                merge_disk_files(&mut aggregate.open_files, &files.open);
                merge_disk_files(&mut aggregate.referenced_files, &files.open);
                merge_disk_files(&mut aggregate.referenced_files, &files.mapped);
            }
            aggregate
                .network_sockets
                .extend(process.sockets.as_deref().into_iter().flatten().copied());
        }
        aggregate.usage.compute.gpu_busy_percent =
            gpu_engines.values().copied().fold(0.0, f64::max);
        aggregate
    }

    fn has_complete_cgroup_attribution(&self, attribution: &ResourceAttribution) -> bool {
        attribution.root_count > 0
            && attribution.cgroup_roots == attribution.root_count
            && attribution.cgroups_cover_process_trees
            && attribution
                .cgroup_paths
                .iter()
                .all(|path| self.cgroup_usage.contains_key(*path))
    }

    fn apply_cgroup_usage(&self, usage: &mut ResourceUsage, paths: &HashSet<&str>) {
        let groups = || paths.iter().filter_map(|path| self.cgroup_usage.get(*path));
        if let Some(cpu) = groups().map(|group| group.cpu_percent).sum::<Option<f64>>() {
            usage.compute.cpu_percent = cpu;
        }
        if let Some(memory) = groups().try_fold(0_u64, |total, group| {
            Some(total.saturating_add(group.memory_bytes?))
        }) {
            usage.compute.memory_cgroup_bytes = memory;
        }
        // A missing controller must not replace valid procfs counters with zero.
        if let Some(io) = groups().try_fold(IoCounters::default(), |mut total, group| {
            let current = group.io?;
            add_counter(&mut total.read_bytes, current.read_bytes);
            add_counter(&mut total.write_bytes, current.write_bytes);
            add_counter(&mut total.read_operations, current.read_operations);
            add_counter(&mut total.write_operations, current.write_operations);
            Some(total)
        }) {
            usage.storage.disk_read_bytes = io.read_bytes;
            usage.storage.disk_write_bytes = io.write_bytes;
            usage.storage.read_operations = io.read_operations;
            usage.storage.write_operations = io.write_operations;
        }
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
        measurement.storage_available = aggregate.storage_processes > 0
            || (complete_cgroup
                && attribution.cgroup_paths.iter().all(|path| {
                    self.cgroup_usage
                        .get(*path)
                        .is_some_and(|usage| usage.io.is_some())
                }));
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
        usage.storage = usage.storage.with_rates(self.interval_seconds);
        usage.network = usage.network.with_rates(self.interval_seconds);
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
            energy_source: available_label(self.energy_source.clone()),
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

impl ProcessUsage {
    fn from_counters(
        process: &ProcessStat,
        previous: Option<&PreviousProcess>,
        io: Option<ProcessIo>,
        system_delta: Option<u64>,
        logical_cpus: usize,
    ) -> Self {
        let previous = previous.filter(|previous| previous.start_ticks == process.start_ticks);
        Self {
            cpu_percent: previous.zip(system_delta).map_or(0.0, |(previous, total)| {
                finite_nonnegative(
                    process.total_ticks.saturating_sub(previous.total_ticks) as f64 / total as f64
                        * logical_cpus as f64
                        * 100.0,
                )
            }),
            major_faults: previous.map_or(0, |previous| {
                process.major_faults.saturating_sub(previous.major_faults)
            }),
            io: previous
                .and_then(|previous| previous.io)
                .zip(io)
                .map_or_else(ProcessIo::default, |(previous, current)| {
                    current.delta(previous)
                }),
            thread_count: process.thread_count,
            storage_available: io.is_some(),
            ..Self::default()
        }
    }

    fn add_to(&self, usage: &mut ResourceUsage) {
        let compute = &mut usage.compute;
        compute.cpu_percent += self.cpu_percent;
        let memory = &self.memory;
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
        add_counter(&mut compute.thread_count, self.thread_count);
        compute.major_faults_per_second += self.major_faults as f64;
        compute.gpu_percent += self.gpu.engines.values().sum::<f64>();
        add_counter(
            &mut compute.gpu_memory_resident_bytes,
            self.gpu.resident_bytes,
        );
        add_counter(
            &mut compute.gpu_memory_allocated_bytes,
            self.gpu.allocated_bytes,
        );
        add_counter(
            &mut compute.gpu_memory_bytes,
            match self.gpu.resident_bytes {
                0 => self.gpu.allocated_bytes,
                resident => resident,
            },
        );

        let storage = &mut usage.storage;
        let io = &self.io;
        add_counter(&mut storage.disk_read_bytes, io.storage.read_bytes);
        add_counter(&mut storage.disk_write_bytes, io.storage.write_bytes);
        add_counter(&mut storage.logical_read_bytes, io.logical_read_bytes);
        add_counter(&mut storage.logical_write_bytes, io.logical_write_bytes);
        add_counter(&mut storage.read_operations, io.storage.read_operations);
        add_counter(&mut storage.write_operations, io.storage.write_operations);
        add_counter(&mut storage.cancelled_write_bytes, io.cancelled_write_bytes);
    }
}

#[derive(Debug)]
pub struct ResourceSampler {
    known_roots: discovery::KnownRoots,
    provider: Arc<dyn ResourceProvider>,
    previous_processes: HashMap<u32, PreviousProcess>,
    gpu: gpu::Sampler,
    previous_system_ticks: Option<u64>,
    previous_cgroups: HashMap<String, CgroupCounters>,
    network: network::Sampler,
    previous_sample: Option<Instant>,
    resume_clock: ResumeDetector,
    memory: ProcessCache<MemoryUsage>,
    open_files: ProcessCache<Arc<ProcessFiles>>,
    app_disk: AppDiskCache,
    energy: EnergySampler,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self {
            known_roots: discovery::KnownRoots::default(),
            provider: Arc::new(LinuxResourceProvider),
            previous_processes: HashMap::new(),
            gpu: gpu::Sampler::default(),
            previous_system_ticks: None,
            previous_cgroups: HashMap::new(),
            network: network::Sampler::default(),
            previous_sample: None,
            resume_clock: ResumeDetector::default(),
            memory: ProcessCache::default(),
            open_files: ProcessCache::default(),
            app_disk: AppDiskCache::default(),
            energy: EnergySampler::default(),
        }
    }
}

type ProcessIdentity = (u32, u64);

#[derive(Debug, Default)]
struct ProcessCache<T> {
    samples: HashMap<ProcessIdentity, T>,
    next_refresh: Option<Instant>,
}

impl ResourceSampler {
    /// Keep disk workers and completed footprints, but never carry counter
    /// deltas or short-lived process caches across an unobserved sleep cycle.
    pub(crate) fn reset_after_resume(&mut self) {
        self.previous_processes.clear();
        self.gpu = gpu::Sampler::default();
        self.previous_system_ticks = None;
        self.previous_cgroups.clear();
        self.network = network::Sampler::default();
        self.previous_sample = None;
        self.energy = EnergySampler::default();
        self.memory = ProcessCache::default();
        self.open_files = ProcessCache::default();
    }

    fn sample_with(
        &mut self,
        discover: impl FnOnce(
            &dyn ResourceProvider,
            &mut discovery::KnownRoots,
        ) -> (ProcessSample, ResourceSnapshot),
    ) -> ResourceSnapshot {
        if self.resume_clock.resumed() {
            self.reset_after_resume();
        }
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
        let (sample, mut snapshot) = discover(provider.as_ref(), &mut self.known_roots);
        snapshot.logical_cpus = logical_cpus;
        snapshot.interval_seconds = interval_seconds;
        let current_cgroups = snapshot
            .cgroup_path_by_root
            .values()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|path| Some((path.clone(), provider.cgroup_counters(path)?)))
            .collect();
        snapshot.cgroup_usage = self.cgroup_usage(&current_cgroups, interval_seconds);
        self.sample_processes(provider.as_ref(), &sample, &mut snapshot, system_delta, now);
        let energy = self.energy.sample(interval_seconds, provider.as_ref());
        snapshot.system_energy_mwh = finite_nonnegative(energy.energy_mwh);
        snapshot.battery_full_mwh = finite_nonnegative(energy.battery_full_mwh);
        snapshot.energy_source = energy.source;
        snapshot.app_disk_by_target =
            self.app_disk
                .read(&provider, snapshot.target_roots.keys(), now);
        self.previous_cgroups = current_cgroups;
        self.previous_system_ticks = Some(system_ticks);
        self.previous_sample = Some(now);
        if self.resume_clock.resumed() {
            // The process may have been frozen halfway through /proc/sysfs
            // reads. Discard that mixed interval rather than persisting it.
            self.reset_after_resume();
            return ResourceSnapshot::default();
        }
        snapshot
    }

    // Expensive reads are restricted to discovered application members. All
    // processes still contribute CPU deltas to the energy-attribution denominator.
    fn sample_processes(
        &mut self,
        provider: &dyn ResourceProvider,
        sample: &ProcessSample,
        snapshot: &mut ResourceSnapshot,
        system_delta: Option<u64>,
        now: Instant,
    ) {
        let ProcessSample {
            processes: current,
            active: active_processes,
            identities,
        } = sample;
        let interval_seconds = snapshot.interval_seconds;
        let logical_cpus = snapshot.logical_cpus;
        let current_gpu = provider.gpu_processes(active_processes);
        self.open_files
            .refresh(identities, now, OPEN_FILE_REFRESH_INTERVAL, |pid| {
                Arc::new(provider.process_files(pid))
            });
        (snapshot.network_deltas, snapshot.network_counters_available) =
            self.network.sample(provider, identities, current);
        self.memory
            .refresh(identities, now, MEMORY_REFRESH_INTERVAL, |pid| {
                provider.process_memory(pid)
            });
        for (&pid, process) in current {
            let identity = (pid, process.start_ticks);
            let sampled_io = active_processes
                .contains(&pid)
                .then(|| provider.process_io(pid))
                .flatten();
            let mut usage = ProcessUsage::from_counters(
                process,
                self.previous_processes.get(&pid),
                sampled_io,
                system_delta,
                logical_cpus,
            );
            usage.memory = self
                .memory
                .samples
                .get(&identity)
                .copied()
                .unwrap_or_default();
            usage.files = self.open_files.samples.get(&identity).cloned();
            usage.sockets = self.network.sockets(identity);
            snapshot.total_process_cpu_percent += usage.cpu_percent;
            snapshot.processes.insert(pid, usage);
            self.previous_processes.insert(
                pid,
                PreviousProcess {
                    total_ticks: process.total_ticks,
                    start_ticks: process.start_ticks,
                    major_faults: process.major_faults,
                    io: sampled_io,
                },
            );
        }
        for (pid, gpu) in self.gpu.sample(
            current_gpu
                .into_iter()
                .filter_map(|(pid, gpu)| Some(((pid, current.get(&pid)?.start_ticks), gpu))),
            interval_seconds,
        ) {
            if let Some(process) = snapshot.processes.get_mut(&pid) {
                process.gpu = gpu;
            }
        }
        self.previous_processes
            .retain(|pid, _| current.contains_key(pid));
    }

    fn cgroup_usage(
        &self,
        current: &HashMap<String, CgroupCounters>,
        seconds: f64,
    ) -> HashMap<String, CgroupUsage> {
        current
            .iter()
            .map(|(path, counters)| {
                let previous = self.previous_cgroups.get(path);
                let usage = CgroupUsage {
                    cpu_percent: counters.cpu_usage_usec.map(|current| {
                        let elapsed = previous
                            .and_then(|value| value.cpu_usage_usec)
                            .map_or(0, |value| current.saturating_sub(value));
                        rate(elapsed as f64, seconds * 10_000.0, 1)
                    }),
                    io: counters.io.map(|current| {
                        previous
                            .and_then(|value| value.io)
                            .map_or_else(IoCounters::default, |previous| current.delta(previous))
                    }),
                    memory_bytes: counters.memory_bytes,
                };
                (path.clone(), usage)
            })
            .collect()
    }
}

impl<T> ProcessCache<T> {
    fn refresh(
        &mut self,
        identities: &HashSet<ProcessIdentity>,
        now: Instant,
        interval: std::time::Duration,
        mut sample: impl FnMut(u32) -> T,
    ) {
        let expired = self.next_refresh.is_none_or(|deadline| now >= deadline);
        self.samples
            .retain(|identity, _| !expired && identities.contains(identity));
        for &identity in identities {
            self.samples
                .entry(identity)
                .or_insert_with(|| sample(identity.0));
        }
        if expired {
            self.next_refresh = Some(now + interval);
        }
    }
}

#[cfg(test)]
mod background_tests;
#[cfg(feature = "benchmarks")]
pub(crate) mod benchmarks;
#[cfg(test)]
mod cache_tests;
#[cfg(any(test, feature = "benchmarks"))]
mod test_provider;
#[cfg(test)]
mod tests;
