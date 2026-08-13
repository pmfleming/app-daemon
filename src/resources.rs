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

#[derive(Debug, Default)]
struct GpuProcessStat {
    engine_nanoseconds: HashMap<String, u64>,
    memory_bytes: u64,
}

#[derive(Debug, Default)]
struct GpuClientStat {
    engine_nanoseconds: HashMap<String, u64>,
    resident_regions: HashMap<String, u64>,
    allocated_regions: HashMap<String, u64>,
}

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

#[derive(Debug, Default)]
struct EnergySampler {
    previous_rapl: HashMap<PathBuf, (u64, u64)>,
}

impl EnergySampler {
    fn sample(&mut self, seconds: f64) -> EnergySample {
        let battery = read_batteries();
        let rapl_mwh = self.rapl_energy_mwh();
        let (energy_mwh, source) = if rapl_mwh > 0.0 {
            (rapl_mwh, "rapl")
        } else if battery.discharge_watts > 0.0 && seconds > 0.0 {
            (battery.discharge_watts * seconds / 3.6, "battery")
        } else {
            (0.0, "unavailable")
        };
        EnergySample {
            energy_mwh,
            battery_full_mwh: battery.full_mwh,
            source: source.into(),
        }
    }

    fn rapl_energy_mwh(&mut self) -> f64 {
        let current = read_rapl_zones();
        let microjoules = current
            .iter()
            .filter_map(|(path, &(value, maximum))| {
                let &(previous, _) = self.previous_rapl.get(path)?;
                Some(counter_delta(previous, value, maximum))
            })
            .sum::<u64>();
        self.previous_rapl = current;
        microjoules as f64 / 3_600_000.0
    }
}

fn counter_delta(previous: u64, current: u64, maximum: u64) -> u64 {
    if current >= previous {
        current - previous
    } else {
        maximum.saturating_sub(previous).saturating_add(current)
    }
}

#[derive(Debug, Default)]
struct EnergySample {
    energy_mwh: f64,
    battery_full_mwh: f64,
    source: String,
}

#[derive(Debug, Default)]
struct BatterySample {
    full_mwh: f64,
    discharge_watts: f64,
}

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

fn read_gpu_processes(pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
    pids.iter()
        .filter_map(|&pid| read_gpu_process(pid).map(|usage| (pid, usage)))
        .collect()
}

fn read_gpu_process(pid: u32) -> Option<GpuProcessStat> {
    let entries = fs::read_dir(format!("/proc/{pid}/fdinfo")).ok()?;
    let clients = entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|value| parse_gpu_fdinfo(&value))
        .fold(
            HashMap::<String, GpuClientStat>::new(),
            |mut clients, (id, client)| {
                clients.entry(id).or_default().merge(client);
                clients
            },
        );
    (!clients.is_empty()).then(|| aggregate_gpu_clients(clients))
}

fn aggregate_gpu_clients(clients: HashMap<String, GpuClientStat>) -> GpuProcessStat {
    let mut process = GpuProcessStat::default();
    for (client_id, client) in clients {
        process.memory_bytes = process.memory_bytes.saturating_add(client.memory_bytes());
        process.engine_nanoseconds.extend(
            client
                .engine_nanoseconds
                .into_iter()
                .map(|(engine, value)| (format!("{client_id}/{engine}"), value)),
        );
    }
    process
}

fn parse_gpu_fdinfo(value: &str) -> Option<(String, GpuClientStat)> {
    let mut client_id = None;
    let mut device = None;
    let mut driver = None;
    let mut client = GpuClientStat::default();
    for (key, value) in drm_fields(value) {
        match key {
            "drm-client-id" => client_id = Some(value),
            "drm-pdev" => device = Some(value),
            "drm-driver" => driver = Some(value),
            _ => client.record(key, value),
        }
    }
    let key = format!("{}/{}", device.or(driver).unwrap_or("unknown"), client_id?);
    (!client.is_empty()).then_some((key, client))
}

fn drm_fields(value: &str) -> impl Iterator<Item = (&str, &str)> {
    value.lines().filter_map(|line| {
        line.split_once(':')
            .map(|(key, value)| (key.trim(), value.trim()))
    })
}

impl GpuClientStat {
    fn record(&mut self, key: &str, value: &str) {
        if record_metric(
            &mut self.engine_nanoseconds,
            key,
            value,
            "drm-engine-",
            parse_duration_nanoseconds,
        ) {
            return;
        }
        if record_metric(
            &mut self.resident_regions,
            key,
            value,
            "drm-resident-",
            parse_bytes,
        ) {
            return;
        }
        record_metric(
            &mut self.allocated_regions,
            key,
            value,
            "drm-memory-",
            parse_bytes,
        );
    }

    fn merge(&mut self, other: Self) {
        merge_max(&mut self.engine_nanoseconds, other.engine_nanoseconds);
        merge_max(&mut self.resident_regions, other.resident_regions);
        merge_max(&mut self.allocated_regions, other.allocated_regions);
    }

    fn is_empty(&self) -> bool {
        self.engine_nanoseconds.is_empty()
            && self.resident_regions.is_empty()
            && self.allocated_regions.is_empty()
    }

    fn memory_bytes(&self) -> u64 {
        let regions = if self.resident_regions.is_empty() {
            &self.allocated_regions
        } else {
            &self.resident_regions
        };
        regions.values().copied().sum()
    }
}

fn record_metric(
    target: &mut HashMap<String, u64>,
    key: &str,
    value: &str,
    prefix: &str,
    parse: fn(&str) -> Option<u64>,
) -> bool {
    let Some(name) = key.strip_prefix(prefix) else {
        return false;
    };
    if let Some(value) = parse(value) {
        target.insert(name.to_owned(), value);
    }
    true
}

fn merge_max(target: &mut HashMap<String, u64>, source: HashMap<String, u64>) {
    for (key, value) in source {
        target
            .entry(key)
            .and_modify(|current| *current = (*current).max(value))
            .or_insert(value);
    }
}

fn parse_duration_nanoseconds(value: &str) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    match fields.next().unwrap_or("ns") {
        "ns" => Some(value),
        "us" => value.checked_mul(1_000),
        "ms" => value.checked_mul(1_000_000),
        _ => None,
    }
}

fn parse_bytes(value: &str) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    let multiplier = match fields.next().unwrap_or("B") {
        "B" => 1,
        "kB" => 1_000,
        "KiB" => 1_024,
        "MB" => 1_000_000,
        "MiB" => 1_048_576,
        "GB" => 1_000_000_000,
        "GiB" => 1_073_741_824,
        _ => return None,
    };
    value.checked_mul(multiplier)
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

fn read_rapl_zones() -> HashMap<PathBuf, (u64, u64)> {
    let root = Path::new("/sys/class/powercap");
    let Ok(entries) = fs::read_dir(root) else {
        return HashMap::new();
    };
    let candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("energy_uj").is_file())
        .collect::<Vec<_>>();
    candidates
        .iter()
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            !candidates.iter().any(|parent| {
                let parent = parent
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                parent != name && name.starts_with(&format!("{parent}:"))
            })
        })
        .filter_map(|path| {
            Some((
                path.clone(),
                (
                    read_u64(&path.join("energy_uj"))?,
                    read_u64(&path.join("max_energy_range_uj")).unwrap_or(u64::MAX),
                ),
            ))
        })
        .collect()
}

fn read_batteries() -> BatterySample {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return BatterySample::default();
    };
    let mut result = BatterySample::default();
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if read_trimmed(&path.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let voltage_uv = read_u64(&path.join("voltage_now")).unwrap_or(0) as f64;
        let full_mwh = read_u64(&path.join("energy_full"))
            .map(|value| value as f64 / 1000.0)
            .or_else(|| {
                read_u64(&path.join("charge_full"))
                    .map(|charge| charge as f64 * voltage_uv / 1_000_000_000.0)
            })
            .unwrap_or(0.0);
        result.full_mwh += full_mwh;
        if read_trimmed(&path.join("status")).as_deref() != Some("Discharging") {
            continue;
        }
        let watts = read_u64(&path.join("power_now"))
            .map(|value| value as f64 / 1_000_000.0)
            .or_else(|| {
                read_u64(&path.join("current_now"))
                    .map(|current| current as f64 * voltage_uv / 1_000_000_000_000.0)
            })
            .unwrap_or(0.0);
        result.discharge_watts += watts;
    }
    result
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
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
    use super::{DiskFileId, ProcessUsage, ResourceSnapshot, parse_gpu_fdinfo, parse_process_stat};
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
    fn rejects_malformed_drm_metrics() {
        assert!(parse_gpu_fdinfo("drm-engine-gfx: nope\n").is_none());
        assert!(parse_gpu_fdinfo("drm-client-id: 4\ndrm-engine-gfx: nope\n").is_none());
    }

    #[test]
    fn handles_energy_counter_rollover() {
        assert_eq!(super::counter_delta(900, 100, 1_000), 200);
        assert_eq!(super::counter_delta(100, 250, 1_000), 150);
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

    #[test]
    fn parses_standard_drm_fdinfo_metrics() -> anyhow::Result<()> {
        let fdinfo = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t7\ndrm-engine-gfx:\t250000000 ns\ndrm-engine-compute:\t10 ms\ndrm-memory-vram:\t64 MiB\ndrm-resident-vram:\t32 MiB\n";
        let (id, client) = parse_gpu_fdinfo(fdinfo).context("DRM client metrics")?;
        assert_eq!(id, "0000:03:00.0/7");
        assert_eq!(client.engine_nanoseconds["gfx"], 250_000_000);
        assert_eq!(client.engine_nanoseconds["compute"], 10_000_000);
        assert_eq!(client.resident_regions["vram"], 32 * 1024 * 1024);
        assert_eq!(client.allocated_regions["vram"], 64 * 1024 * 1024);
        Ok(())
    }
}
