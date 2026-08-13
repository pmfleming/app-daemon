use std::{
    collections::{HashMap, HashSet},
    fs,
};

#[derive(Debug, Default)]
pub(super) struct GpuProcessStat {
    pub(super) engine_nanoseconds: HashMap<String, u64>,
    pub(super) memory_bytes: u64,
}

#[derive(Debug, Default)]
struct GpuClientStat {
    engine_nanoseconds: HashMap<String, u64>,
    resident_regions: HashMap<String, u64>,
    allocated_regions: HashMap<String, u64>,
}

pub(super) fn read_gpu_processes(pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
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

#[cfg(test)]
mod tests {
    use super::parse_gpu_fdinfo;
    use anyhow::Context;

    #[test]
    fn rejects_malformed_metrics() {
        assert!(parse_gpu_fdinfo("drm-engine-gfx: nope\n").is_none());
        assert!(parse_gpu_fdinfo("drm-client-id: 4\ndrm-engine-gfx: nope\n").is_none());
    }

    #[test]
    fn parses_standard_metrics() -> anyhow::Result<()> {
        let value = "drm-pdev: 0000:03:00.0\ndrm-client-id: 7\ndrm-engine-gfx: 250000000 ns\ndrm-engine-compute: 10 ms\ndrm-memory-vram: 64 MiB\ndrm-resident-vram: 32 MiB\n";
        let (id, client) = parse_gpu_fdinfo(value).context("DRM client metrics")?;
        assert_eq!(id, "0000:03:00.0/7");
        assert_eq!(client.engine_nanoseconds["gfx"], 250_000_000);
        assert_eq!(client.engine_nanoseconds["compute"], 10_000_000);
        assert_eq!(client.resident_regions["vram"], 32 * 1024 * 1024);
        assert_eq!(client.allocated_regions["vram"], 64 * 1024 * 1024);
        Ok(())
    }
}
