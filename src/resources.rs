use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceUsage {
    /// Top-compatible CPU usage: 100% is one fully occupied logical CPU.
    pub cpu_percent: f64,
    /// CPU usage as a percentage of the whole machine, always capped at 100%.
    pub cpu_percent_of_machine: f64,
    pub memory_bytes: u64,
    /// DRM engine time: 100% is one fully occupied GPU engine.
    pub gpu_percent: f64,
    /// Resident GPU memory reported by DRM, falling back to allocated memory.
    pub gpu_memory_bytes: u64,
    /// Estimated energy attributed to the process tree during this sample.
    pub energy_mwh: f64,
    /// The energy estimate expressed as a percentage of full battery capacity.
    pub battery_percent: f64,
    pub power_watts: f64,
    pub battery_percent_per_hour: f64,
    pub energy_source: String,
}

#[derive(Debug, Clone, Copy)]
struct ProcessStat {
    parent_pid: u32,
    total_ticks: u64,
    start_ticks: u64,
    memory_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct PreviousProcess {
    total_ticks: u64,
    start_ticks: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProcessUsage {
    parent_pid: u32,
    cpu_percent: f64,
    memory_bytes: u64,
    gpu_percent: f64,
    gpu_memory_bytes: u64,
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
        let mut pending = roots.into_iter().filter(|pid| *pid > 0).collect::<Vec<_>>();
        let mut included = HashSet::new();
        while let Some(pid) = pending.pop() {
            if !included.insert(pid) {
                continue;
            }
            if let Some(children) = self.children.get(&pid) {
                pending.extend(children);
            }
        }

        let mut usage = ResourceUsage::default();
        for pid in included {
            if let Some(process) = self.processes.get(&pid) {
                usage.cpu_percent += process.cpu_percent;
                usage.memory_bytes = usage.memory_bytes.saturating_add(process.memory_bytes);
                usage.gpu_percent += process.gpu_percent;
                usage.gpu_memory_bytes = usage
                    .gpu_memory_bytes
                    .saturating_add(process.gpu_memory_bytes);
            }
        }

        let raw_cpu = usage.cpu_percent.max(0.0);
        usage.cpu_percent = rounded(raw_cpu, 1);
        usage.cpu_percent_of_machine = rounded(
            (raw_cpu / self.logical_cpus.max(1) as f64).clamp(0.0, 100.0),
            1,
        );
        let raw_gpu = usage.gpu_percent.max(0.0);
        usage.gpu_percent = rounded(raw_gpu, 1);
        let total_activity = self.total_process_cpu_percent + self.total_process_gpu_percent;
        let share = if total_activity > 0.0 {
            ((raw_cpu + raw_gpu) / total_activity).clamp(0.0, 1.0)
        } else {
            0.0
        };
        usage.energy_mwh = rounded(self.system_energy_mwh * share, 4);
        usage.power_watts = if self.interval_seconds > 0.0 {
            rounded(usage.energy_mwh * 3.6 / self.interval_seconds, 3)
        } else {
            0.0
        };
        if self.battery_full_mwh > 0.0 {
            usage.battery_percent = rounded(usage.energy_mwh / self.battery_full_mwh * 100.0, 6);
            usage.battery_percent_per_hour = rounded(
                usage.power_watts / (self.battery_full_mwh / 1000.0) * 100.0,
                4,
            );
        }
        usage.energy_source = self.energy_source.clone();
        usage
    }

    pub fn interval_seconds(&self) -> f64 {
        self.interval_seconds
    }
}

#[derive(Debug, Default)]
struct EnergySampler {
    previous_rapl: HashMap<PathBuf, (u64, u64)>,
}

impl EnergySampler {
    fn sample(&mut self, interval_seconds: f64) -> EnergySample {
        let battery = read_batteries();
        let current = read_rapl_zones();
        let mut rapl_uj = 0_u64;
        let mut matched = false;
        for (path, &(value, maximum)) in &current {
            let Some(&(previous, _)) = self.previous_rapl.get(path) else {
                continue;
            };
            matched = true;
            let delta = if value >= previous {
                value - previous
            } else if maximum > previous {
                maximum - previous + value
            } else {
                0
            };
            rapl_uj = rapl_uj.saturating_add(delta);
        }
        self.previous_rapl = current;

        if matched && rapl_uj > 0 {
            return EnergySample {
                energy_mwh: rapl_uj as f64 / 3_600_000.0,
                battery_full_mwh: battery.full_mwh,
                source: "rapl".into(),
            };
        }
        if battery.discharge_watts > 0.0 && interval_seconds > 0.0 {
            return EnergySample {
                energy_mwh: battery.discharge_watts * interval_seconds / 3.6,
                battery_full_mwh: battery.full_mwh,
                source: "battery".into(),
            };
        }
        EnergySample {
            battery_full_mwh: battery.full_mwh,
            ..EnergySample::default()
        }
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
    pub fn sample(&mut self) -> ResourceSnapshot {
        self.sample_for_roots(std::iter::empty())
    }

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
            let cpu_percent = system_delta
                .and_then(|total_delta| {
                    let previous = self.previous_processes.get(&pid)?;
                    (previous.start_ticks == process.start_ticks).then(|| {
                        process.total_ticks.saturating_sub(previous.total_ticks) as f64
                            / total_delta as f64
                            * logical_cpus as f64
                            * 100.0
                    })
                })
                .map(finite_nonnegative)
                .unwrap_or(0.0);
            let gpu = current_gpu.get(&pid);
            let gpu_percent = gpu
                .map(|gpu| {
                    gpu.engine_nanoseconds
                        .iter()
                        .map(|(engine, &nanoseconds)| {
                            let key = (pid, process.start_ticks, engine.clone());
                            let previous = self.previous_gpu_engines.get(&key).copied();
                            next_gpu_engines.insert(key, nanoseconds);
                            previous
                                .map(|previous| nanoseconds.saturating_sub(previous))
                                .unwrap_or(0)
                        })
                        .sum::<u64>() as f64
                        / (interval_seconds * 1_000_000_000.0)
                        * 100.0
                })
                .filter(|_| interval_seconds > 0.0)
                .map(finite_nonnegative)
                .unwrap_or(0.0);
            snapshot.total_process_cpu_percent += cpu_percent;
            snapshot.total_process_gpu_percent += gpu_percent;
            snapshot.processes.insert(
                pid,
                ProcessUsage {
                    parent_pid: process.parent_pid,
                    cpu_percent,
                    memory_bytes: process.memory_bytes,
                    gpu_percent,
                    gpu_memory_bytes: gpu.map_or(0, |gpu| gpu.memory_bytes),
                },
            );
        }
        for (&pid, process) in &snapshot.processes {
            snapshot
                .children
                .entry(process.parent_pid)
                .or_default()
                .push(pid);
        }

        self.previous_processes = current
            .into_iter()
            .map(|(pid, process)| {
                (
                    pid,
                    PreviousProcess {
                        total_ticks: process.total_ticks,
                        start_ticks: process.start_ticks,
                    },
                )
            })
            .collect();
        self.previous_gpu_engines = next_gpu_engines;
        self.previous_system_ticks = Some(system_ticks);
        self.previous_sample = Some(now);
        snapshot
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
            parse_process_stat(&stat, memory_bytes).map(|process| (pid, process))
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

fn read_gpu_processes(pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
    pids.iter()
        .filter_map(|&pid| read_gpu_process(pid).map(|usage| (pid, usage)))
        .collect()
}

fn read_gpu_process(pid: u32) -> Option<GpuProcessStat> {
    let entries = fs::read_dir(format!("/proc/{pid}/fdinfo")).ok()?;
    let mut clients: HashMap<String, GpuClientStat> = HashMap::new();
    for fdinfo in entries.filter_map(Result::ok) {
        let Ok(value) = fs::read_to_string(fdinfo.path()) else {
            continue;
        };
        let Some((id, client)) = parse_gpu_fdinfo(&value) else {
            continue;
        };
        let target = clients.entry(id).or_default();
        merge_max(&mut target.engine_nanoseconds, client.engine_nanoseconds);
        merge_max(&mut target.resident_regions, client.resident_regions);
        merge_max(&mut target.allocated_regions, client.allocated_regions);
    }
    if clients.is_empty() {
        return None;
    }
    let mut process = GpuProcessStat::default();
    for (client_id, client) in clients {
        for (engine, value) in client.engine_nanoseconds {
            process
                .engine_nanoseconds
                .insert(format!("{client_id}/{engine}"), value);
        }
        let regions = if client.resident_regions.is_empty() {
            client.allocated_regions
        } else {
            client.resident_regions
        };
        process.memory_bytes = process
            .memory_bytes
            .saturating_add(regions.values().copied().sum::<u64>());
    }
    Some(process)
}

fn parse_gpu_fdinfo(value: &str) -> Option<(String, GpuClientStat)> {
    let fields = value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect::<Vec<_>>();
    let client_id = fields
        .iter()
        .find_map(|(key, value)| (*key == "drm-client-id").then_some(*value))?;
    let device = fields
        .iter()
        .find_map(|(key, value)| (*key == "drm-pdev").then_some(*value))
        .or_else(|| {
            fields
                .iter()
                .find_map(|(key, value)| (*key == "drm-driver").then_some(*value))
        })
        .unwrap_or("unknown");
    let mut client = GpuClientStat::default();
    for (key, value) in fields {
        if let Some(engine) = key.strip_prefix("drm-engine-") {
            if let Some(nanoseconds) = parse_duration_nanoseconds(value) {
                client
                    .engine_nanoseconds
                    .insert(engine.to_owned(), nanoseconds);
            }
        } else if let Some(region) = key.strip_prefix("drm-resident-") {
            if let Some(bytes) = parse_bytes(value) {
                client.resident_regions.insert(region.to_owned(), bytes);
            }
        } else if let Some(region) = key.strip_prefix("drm-memory-")
            && let Some(bytes) = parse_bytes(value)
        {
            client.allocated_regions.insert(region.to_owned(), bytes);
        }
    }
    (!client.engine_nanoseconds.is_empty()
        || !client.resident_regions.is_empty()
        || !client.allocated_regions.is_empty())
    .then(|| (format!("{device}/{client_id}"), client))
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

fn parse_process_stat(value: &str, memory_bytes: u64) -> Option<ProcessStat> {
    let command_end = value.rfind(')')?;
    let fields = value
        .get(command_end + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessStat {
        parent_pid: fields.get(1)?.parse().ok()?,
        total_ticks: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .saturating_add(fields.get(12)?.parse::<u64>().ok()?),
        start_ticks: fields.get(19)?.parse().ok()?,
        memory_bytes,
    })
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

fn rounded(value: f64, decimals: i32) -> f64 {
    let value = finite_nonnegative(value);
    let scale = 10_f64.powi(decimals);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::{ProcessUsage, ResourceSnapshot, parse_gpu_fdinfo, parse_process_stat};
    use std::collections::HashMap;

    #[test]
    fn parses_proc_stat_with_spaces_in_command() {
        let stat = "42 (application helper) S 7 0 0 0 0 0 0 0 0 0 120 30 0 0 0 0 0 0 99 0 0";
        let process = parse_process_stat(stat, 4096).expect("valid stat");
        assert_eq!(process.parent_pid, 7);
        assert_eq!(process.total_ticks, 150);
        assert_eq!(process.start_ticks, 99);
        assert_eq!(process.memory_bytes, 4096);
    }

    #[test]
    fn totals_process_trees_without_double_counting_shared_roots() {
        let processes = HashMap::from([
            (
                10,
                ProcessUsage {
                    parent_pid: 1,
                    cpu_percent: 2.0,
                    memory_bytes: 100,
                    gpu_percent: 0.0,
                    gpu_memory_bytes: 0,
                },
            ),
            (
                11,
                ProcessUsage {
                    parent_pid: 10,
                    cpu_percent: 3.5,
                    memory_bytes: 200,
                    gpu_percent: 0.0,
                    gpu_memory_bytes: 0,
                },
            ),
            (
                20,
                ProcessUsage {
                    parent_pid: 1,
                    cpu_percent: 1.0,
                    memory_bytes: 50,
                    gpu_percent: 0.0,
                    gpu_memory_bytes: 0,
                },
            ),
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
        assert_eq!(usage.cpu_percent, 5.5);
        assert_eq!(usage.cpu_percent_of_machine, 1.4);
        assert_eq!(usage.memory_bytes, 300);
        assert_eq!(usage.energy_mwh, 2.75);
        assert_eq!(usage.energy_source, "rapl");
    }

    #[test]
    fn parses_standard_drm_fdinfo_metrics() {
        let fdinfo = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t7\ndrm-engine-gfx:\t250000000 ns\ndrm-engine-compute:\t10 ms\ndrm-memory-vram:\t64 MiB\ndrm-resident-vram:\t32 MiB\n";
        let (id, client) = parse_gpu_fdinfo(fdinfo).expect("DRM client metrics");
        assert_eq!(id, "0000:03:00.0/7");
        assert_eq!(client.engine_nanoseconds["gfx"], 250_000_000);
        assert_eq!(client.engine_nanoseconds["compute"], 10_000_000);
        assert_eq!(client.resident_regions["vram"], 32 * 1024 * 1024);
        assert_eq!(client.allocated_regions["vram"], 64 * 1024 * 1024);
    }
}
