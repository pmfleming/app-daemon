use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs,
};

use super::provider::GpuProcessStat;
use crate::{metrics::finite_nonnegative, process::Identity};

#[derive(Debug, Clone, Default)]
pub(super) struct Usage {
    pub available: bool,
    pub engines: HashMap<String, f64>,
    pub resident_bytes: u64,
    pub allocated_bytes: u64,
}

#[derive(Debug, Default)]
pub(super) struct Sampler {
    previous: HashMap<Identity, HashMap<String, u64>>,
}

impl Sampler {
    pub(super) fn sample(
        &mut self,
        current: impl IntoIterator<Item = (Identity, GpuProcessStat)>,
        seconds: f64,
    ) -> impl Iterator<Item = (u32, Usage)> {
        // Consume the whole frame. Missing observations must not bridge gaps.
        let previous = std::mem::take(&mut self.previous);
        current.into_iter().map(move |(identity, gpu)| {
            let usage = Usage {
                available: true,
                engines: engine_percent(previous.get(&identity), &gpu.engine_nanoseconds, seconds),
                resident_bytes: gpu.resident_memory_bytes,
                allocated_bytes: gpu.allocated_memory_bytes,
            };
            // Move the counters and their keys instead of cloning every engine name.
            self.previous.insert(identity, gpu.engine_nanoseconds);
            (identity.0, usage)
        })
    }
}

fn engine_percent(
    previous: Option<&HashMap<String, u64>>,
    current: &HashMap<String, u64>,
    seconds: f64,
) -> HashMap<String, f64> {
    let mut engines = HashMap::new();
    for (client_engine, &nanoseconds) in current {
        let elapsed = previous
            .and_then(|values| values.get(client_engine))
            .map_or(0, |&value| nanoseconds.saturating_sub(value));
        let percent = if seconds > 0.0 {
            finite_nonnegative(elapsed as f64 / (seconds * 1_000_000_000.0) * 100.0)
        } else {
            0.0
        };
        *engines.entry(engine_scope(client_engine)).or_default() += percent;
    }
    engines
}

/// Sum occupancy by device/engine, not client or across unrelated GPUs.
fn engine_scope(client_engine: &str) -> String {
    let (client, engine) = client_engine
        .rsplit_once('/')
        .unwrap_or(("unknown/unknown", client_engine));
    let (device, _) = client.rsplit_once('/').unwrap_or((client, "unknown"));
    format!("{device}/{engine}")
}

#[derive(Debug, Default)]
struct GpuClientStat {
    engine_nanoseconds: HashMap<String, u64>,
    resident_regions: HashMap<String, u64>,
    allocated_regions: HashMap<String, u64>,
}

pub(super) fn read_gpu_processes(pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
    let mut clients = pids
        .iter()
        .map(|&pid| (pid, read_gpu_clients(pid)))
        .collect::<Vec<_>>();
    clients.sort_unstable_by_key(|(pid, _)| *pid);
    let mut processes = HashMap::<u32, GpuProcessStat>::new();
    for (id, (pid, client)) in deduplicate_clients(clients) {
        processes.entry(pid).or_default().add_client(id, client);
    }
    processes
}

fn deduplicate_clients(
    clients: Vec<(u32, HashMap<String, GpuClientStat>)>,
) -> HashMap<String, (u32, GpuClientStat)> {
    let mut unique = HashMap::<String, (u32, GpuClientStat)>::new();
    for (pid, clients) in clients {
        for (id, client) in clients {
            match unique.entry(id) {
                Entry::Occupied(mut entry) => entry.get_mut().1.merge(client),
                Entry::Vacant(entry) => {
                    entry.insert((pid, client));
                }
            }
        }
    }
    unique
}

fn read_gpu_clients(pid: u32) -> HashMap<String, GpuClientStat> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|value| parse_gpu_fdinfo(&value))
        .fold(
            HashMap::<String, GpuClientStat>::new(),
            |mut clients, (id, client)| {
                clients.entry(id).or_default().merge(client);
                clients
            },
        )
}

impl crate::resources::provider::GpuProcessStat {
    fn add_client(&mut self, client_id: String, client: GpuClientStat) {
        self.resident_memory_bytes = self
            .resident_memory_bytes
            .saturating_add(client.resident_regions.values().sum());
        self.allocated_memory_bytes = self
            .allocated_memory_bytes
            .saturating_add(client.allocated_regions.values().sum());
        self.engine_nanoseconds.extend(
            client
                .engine_nanoseconds
                .into_iter()
                .map(|(engine, value)| (format!("{client_id}/{engine}"), value)),
        );
    }
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
        for (prefix, metrics, parse) in [
            (
                "drm-engine-",
                &mut self.engine_nanoseconds,
                parse_duration_nanoseconds as fn(&str) -> Option<u64>,
            ),
            ("drm-resident-", &mut self.resident_regions, parse_bytes),
            ("drm-memory-", &mut self.allocated_regions, parse_bytes),
        ] {
            if record_metric(metrics, key, value, prefix, parse) {
                return;
            }
        }
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

const TIME_UNITS: &[(&str, u64)] = &[("ns", 1), ("us", 1_000), ("ms", 1_000_000)];
const BYTE_UNITS: &[(&str, u64)] = &[
    ("B", 1),
    ("kB", 1_000),
    ("KiB", 1_024),
    ("MB", 1_000_000),
    ("MiB", 1_048_576),
    ("GB", 1_000_000_000),
    ("GiB", 1_073_741_824),
];

fn parse_duration_nanoseconds(value: &str) -> Option<u64> {
    parse_scaled(value, "ns", TIME_UNITS)
}

fn parse_bytes(value: &str) -> Option<u64> {
    parse_scaled(value, "B", BYTE_UNITS)
}

fn parse_scaled(value: &str, default_unit: &str, units: &[(&str, u64)]) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    let unit = fields.next().unwrap_or(default_unit);
    let multiplier = units
        .iter()
        .find_map(|(candidate, multiplier)| (*candidate == unit).then_some(*multiplier))?;
    value.checked_mul(multiplier)
}
