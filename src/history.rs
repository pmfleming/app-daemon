use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{model::ResourceHistoryPoint, resources::ResourceUsage};

const FILE_VERSION: u8 = 1;
const BUCKET_MILLISECONDS: u64 = 15_000;
const RETENTION_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Default)]
struct PendingPoint {
    timestamp_ms: u64,
    duration_ms: u64,
    cpu_weighted: f64,
    machine_cpu_weighted: f64,
    memory_weighted: f64,
    gpu_weighted: f64,
    gpu_memory_weighted: f64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    open_file_disk_weighted: f64,
    energy_mwh: f64,
    battery_percent: f64,
    energy_source: String,
}

impl PendingPoint {
    fn add(&mut self, timestamp_ms: u64, duration_ms: u64, usage: &ResourceUsage) {
        if self.timestamp_ms == 0 {
            self.timestamp_ms = timestamp_ms.saturating_sub(duration_ms);
        }
        self.duration_ms = self.duration_ms.saturating_add(duration_ms);
        self.cpu_weighted += usage.cpu_percent * duration_ms as f64;
        self.machine_cpu_weighted += usage.cpu_percent_of_machine * duration_ms as f64;
        self.memory_weighted += usage.memory_bytes as f64 * duration_ms as f64;
        self.gpu_weighted += usage.gpu_percent * duration_ms as f64;
        self.gpu_memory_weighted += usage.gpu_memory_bytes as f64 * duration_ms as f64;
        self.disk_read_bytes = self.disk_read_bytes.saturating_add(usage.disk_read_bytes);
        self.disk_write_bytes = self.disk_write_bytes.saturating_add(usage.disk_write_bytes);
        self.open_file_disk_weighted += usage.open_file_disk_bytes as f64 * duration_ms as f64;
        self.energy_mwh += usage.energy_mwh;
        self.battery_percent += usage.battery_percent;
        if usage.energy_source != "unavailable" {
            self.energy_source.clone_from(&usage.energy_source);
        }
    }

    fn finish(self) -> Option<ResourceHistoryPoint> {
        if self.duration_ms == 0 {
            return None;
        }
        let duration = self.duration_ms as f64;
        Some(ResourceHistoryPoint {
            timestamp_ms: self.timestamp_ms.saturating_add(self.duration_ms),
            duration_ms: self.duration_ms,
            cpu_percent: rounded(self.cpu_weighted / duration, 1),
            cpu_percent_of_machine: rounded(self.machine_cpu_weighted / duration, 1),
            memory_bytes: (self.memory_weighted / duration).round() as u64,
            gpu_percent: rounded(self.gpu_weighted / duration, 1),
            gpu_memory_bytes: (self.gpu_memory_weighted / duration).round() as u64,
            disk_read_bytes: self.disk_read_bytes,
            disk_write_bytes: self.disk_write_bytes,
            disk_read_bytes_per_second: rounded(self.disk_read_bytes as f64 * 1000.0 / duration, 1),
            disk_write_bytes_per_second: rounded(
                self.disk_write_bytes as f64 * 1000.0 / duration,
                1,
            ),
            open_file_disk_bytes: (self.open_file_disk_weighted / duration).round() as u64,
            energy_mwh: rounded(self.energy_mwh, 4),
            battery_percent: rounded(self.battery_percent, 6),
            average_power_watts: rounded(self.energy_mwh * 3_600.0 / duration, 3),
            energy_source: if self.energy_source.is_empty() {
                "unavailable".into()
            } else {
                self.energy_source
            },
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoryFile {
    version: u8,
    applications: HashMap<String, Vec<ResourceHistoryPoint>>,
}

#[derive(Debug)]
pub struct HistoryStore {
    path: Option<PathBuf>,
    points: HashMap<String, VecDeque<ResourceHistoryPoint>>,
    pending: HashMap<String, PendingPoint>,
}

impl HistoryStore {
    pub fn load_default() -> Self {
        Self::load(history_path())
    }

    fn load(path: Option<PathBuf>) -> Self {
        let points = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<HistoryFile>(&bytes).ok())
            .filter(|file| file.version == FILE_VERSION)
            .map(|file| {
                file.applications
                    .into_iter()
                    .map(|(id, points)| (id, points.into()))
                    .collect()
            })
            .unwrap_or_default();
        let mut store = Self {
            path,
            points,
            pending: HashMap::new(),
        };
        store.prune(now_milliseconds());
        store
    }

    pub fn record(
        &mut self,
        target_id: &str,
        timestamp_ms: u64,
        duration_seconds: f64,
        usage: &ResourceUsage,
    ) {
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return;
        }
        let duration_ms = (duration_seconds * 1000.0).round().max(1.0) as u64;
        let pending = self.pending.entry(target_id.to_owned()).or_default();
        pending.add(timestamp_ms, duration_ms, usage);
        if pending.duration_ms >= BUCKET_MILLISECONDS {
            let point = std::mem::take(pending).finish();
            if let Some(point) = point {
                self.points
                    .entry(target_id.to_owned())
                    .or_default()
                    .push_back(point);
            }
        }
        self.prune(timestamp_ms);
    }

    pub fn query(
        &self,
        target_id: &str,
        since_ms: Option<u64>,
        limit: usize,
    ) -> (Vec<ResourceHistoryPoint>, bool) {
        let Some(points) = self.points.get(target_id) else {
            return (Vec::new(), false);
        };
        let matching = points
            .iter()
            .filter(|point| since_ms.is_none_or(|since| point.timestamp_ms >= since))
            .collect::<Vec<_>>();
        let limit = limit.clamp(1, 10_000);
        let has_more = matching.len() > limit;
        let skip = matching.len().saturating_sub(limit);
        (
            matching.into_iter().skip(skip).cloned().collect::<Vec<_>>(),
            has_more,
        )
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.flush_pending();
        self.prune(now_milliseconds());
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = HistoryFile {
            version: FILE_VERSION,
            applications: self
                .points
                .iter()
                .map(|(id, points)| (id.clone(), points.iter().cloned().collect()))
                .collect(),
        };
        let bytes = serde_json::to_vec(&file)?;
        let temporary = temporary_path(path);
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
    }

    fn flush_pending(&mut self) {
        for (id, pending) in std::mem::take(&mut self.pending) {
            if let Some(point) = pending.finish() {
                self.points.entry(id).or_default().push_back(point);
            }
        }
    }

    fn prune(&mut self, timestamp_ms: u64) {
        let cutoff = timestamp_ms.saturating_sub(RETENTION_MILLISECONDS);
        self.points.retain(|_, points| {
            while points
                .front()
                .is_some_and(|point| point.timestamp_ms < cutoff)
            {
                points.pop_front();
            }
            !points.is_empty()
        });
    }
}

pub fn now_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn history_path() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|root| root.join("app-daemon/resource-history-v1.json"))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    PathBuf::from(value)
}

fn rounded(value: f64, decimals: i32) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }
    let scale = 10_f64.powi(decimals);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::HistoryStore;
    use crate::resources::ResourceUsage;

    #[test]
    fn aggregates_and_persists_resource_buckets() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("history.json");
        let mut store = HistoryStore::load(Some(path.clone()));
        let usage = ResourceUsage {
            cpu_percent: 50.0,
            cpu_percent_of_machine: 12.5,
            memory_bytes: 1024,
            gpu_percent: 25.0,
            gpu_memory_bytes: 2048,
            disk_read_bytes: 100,
            disk_write_bytes: 200,
            disk_read_bytes_per_second: 20.0,
            disk_write_bytes_per_second: 40.0,
            open_file_disk_bytes: 4096,
            energy_mwh: 2.0,
            battery_percent: 0.004,
            power_watts: 3.6,
            battery_percent_per_hour: 7.2,
            energy_source: "rapl".into(),
        };
        let now = super::now_milliseconds();
        for timestamp in [now - 10_000, now - 5_000, now] {
            store.record("example.desktop", timestamp, 5.0, &usage);
        }
        let (points, more) = store.query("example.desktop", None, 10);
        assert!(!more);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].cpu_percent, 50.0);
        assert_eq!(points[0].gpu_percent, 25.0);
        assert_eq!(points[0].gpu_memory_bytes, 2048);
        assert_eq!(points[0].disk_read_bytes, 300);
        assert_eq!(points[0].disk_write_bytes, 600);
        assert_eq!(points[0].disk_read_bytes_per_second, 20.0);
        assert_eq!(points[0].disk_write_bytes_per_second, 40.0);
        assert_eq!(points[0].open_file_disk_bytes, 4096);
        assert_eq!(points[0].energy_mwh, 6.0);
        store.save()?;

        let loaded = HistoryStore::load(Some(path));
        assert_eq!(loaded.query("example.desktop", None, 10).0, points);
        Ok(())
    }
}
