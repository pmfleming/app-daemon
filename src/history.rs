use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::model::{
    ComputeUsage, HistoricalResourceUsage, NetworkUsage, ResourceHistoryPoint, ResourcePeaks,
    ResourceUsage, StorageUsage,
};

const FILE_VERSION: u8 = 1;
const BUCKET_MILLISECONDS: u64 = 15_000;
const RETENTION_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Default)]
struct PendingPoint {
    timestamp_ms: u64,
    duration_ms: u64,
    compute: WeightedCompute,
    storage: WeightedStorage,
    network: WeightedNetwork,
    energy_mwh: f64,
    battery_percent: f64,
    system_power: f64,
    attributed_fraction: f64,
    coverage: f64,
    sample_count: u64,
    energy_source: String,
    energy_confidence: String,
    peaks: ResourcePeaks,
}

#[derive(Debug, Default)]
struct WeightedCompute {
    cpu: f64,
    machine_cpu: f64,
    memory: f64,
    memory_rss: f64,
    memory_pss: f64,
    memory_private: f64,
    memory_swap: f64,
    memory_cgroup: f64,
    process_count: f64,
    thread_count: f64,
    major_faults: f64,
    gpu: f64,
    gpu_busy: f64,
    gpu_memory: f64,
    gpu_memory_resident: f64,
    gpu_memory_allocated: f64,
}

#[derive(Debug, Default)]
struct WeightedStorage {
    read_bytes: u64,
    write_bytes: u64,
    logical_read_bytes: u64,
    logical_write_bytes: u64,
    read_operations: u64,
    write_operations: u64,
    cancelled_write_bytes: u64,
    open_file_bytes: f64,
    referenced_file_disk: f64,
    referenced_file_temporary: f64,
    referenced_file_permanent: f64,
    disk_space_total: f64,
    disk_space_temporary: f64,
    disk_space_permanent: f64,
}

#[derive(Debug, Default)]
struct WeightedNetwork {
    receive_bytes: u64,
    transmit_bytes: u64,
    connection_count: f64,
}

impl PendingPoint {
    fn add(&mut self, duration_ms: u64, usage: &ResourceUsage) {
        self.duration_ms = self.duration_ms.saturating_add(duration_ms);
        self.compute.add(duration_ms, &usage.compute);
        self.storage.add(duration_ms, &usage.storage);
        self.network.add(duration_ms, &usage.network);
        self.energy_mwh += usage.energy.energy_mwh;
        self.battery_percent += usage.energy.battery_percent;
        let weight = duration_ms as f64;
        self.system_power += usage.energy.system_power_watts * weight;
        self.attributed_fraction += usage.energy.attributed_fraction * weight;
        self.coverage += usage.measurement.coverage * weight;
        self.sample_count = self.sample_count.saturating_add(1);
        merge_label(&mut self.energy_source, &usage.energy.energy_source);
        merge_label(&mut self.energy_confidence, &usage.energy.energy_confidence);
        self.peaks.cpu_percent = self.peaks.cpu_percent.max(usage.compute.cpu_percent);
        self.peaks.cpu_percent_of_machine = self
            .peaks
            .cpu_percent_of_machine
            .max(usage.compute.cpu_percent_of_machine);
        self.peaks.memory_bytes = self.peaks.memory_bytes.max(usage.compute.memory_bytes);
        self.peaks.gpu_percent = self.peaks.gpu_percent.max(usage.compute.gpu_percent);
        self.peaks.gpu_busy_percent = self
            .peaks
            .gpu_busy_percent
            .max(usage.compute.gpu_busy_percent);
        self.peaks.disk_read_bytes_per_second = self
            .peaks
            .disk_read_bytes_per_second
            .max(usage.storage.disk_read_bytes_per_second);
        self.peaks.disk_write_bytes_per_second = self
            .peaks
            .disk_write_bytes_per_second
            .max(usage.storage.disk_write_bytes_per_second);
        self.peaks.estimated_app_power_watts = self
            .peaks
            .estimated_app_power_watts
            .max(usage.energy.estimated_app_power_watts);
    }

    fn finish(self) -> Option<ResourceHistoryPoint> {
        if self.duration_ms == 0 {
            return None;
        }
        let duration = self.duration_ms as f64;
        Some(ResourceHistoryPoint {
            timestamp_ms: self.timestamp_ms.saturating_add(BUCKET_MILLISECONDS),
            duration_ms: self.duration_ms,
            resources: HistoricalResourceUsage {
                compute: self.compute.finish(duration),
                storage: self.storage.finish(duration),
                network: self.network.finish(duration),
                energy_mwh: rounded(self.energy_mwh, 4),
                battery_percent: rounded(self.battery_percent, 6),
                average_power_watts: rounded(self.energy_mwh * 3_600.0 / duration, 3),
                system_power_watts: rounded(self.system_power / duration, 3),
                attributed_fraction: rounded(self.attributed_fraction / duration, 4),
                energy_source: available_source(self.energy_source),
                energy_confidence: available_source(self.energy_confidence),
                sample_count: self.sample_count,
                coverage: rounded(self.coverage / duration, 4),
                peaks: self.peaks,
            },
        })
    }
}

impl WeightedCompute {
    fn add(&mut self, duration_ms: u64, usage: &ComputeUsage) {
        let weight = duration_ms as f64;
        self.cpu += usage.cpu_percent * weight;
        self.machine_cpu += usage.cpu_percent_of_machine * weight;
        self.memory += usage.memory_bytes as f64 * weight;
        self.memory_rss += usage.memory_rss_bytes as f64 * weight;
        self.memory_pss += usage.memory_pss_bytes as f64 * weight;
        self.memory_private += usage.memory_private_bytes as f64 * weight;
        self.memory_swap += usage.memory_swap_bytes as f64 * weight;
        self.memory_cgroup += usage.memory_cgroup_bytes as f64 * weight;
        self.process_count += usage.process_count as f64 * weight;
        self.thread_count += usage.thread_count as f64 * weight;
        self.major_faults += usage.major_faults_per_second * weight;
        self.gpu += usage.gpu_percent * weight;
        self.gpu_busy += usage.gpu_busy_percent * weight;
        self.gpu_memory += usage.gpu_memory_bytes as f64 * weight;
        self.gpu_memory_resident += usage.gpu_memory_resident_bytes as f64 * weight;
        self.gpu_memory_allocated += usage.gpu_memory_allocated_bytes as f64 * weight;
    }

    fn finish(self, duration: f64) -> ComputeUsage {
        ComputeUsage {
            cpu_percent: rounded(self.cpu / duration, 1),
            cpu_percent_of_machine: rounded(self.machine_cpu / duration, 1),
            memory_bytes: (self.memory / duration).round() as u64,
            memory_rss_bytes: (self.memory_rss / duration).round() as u64,
            memory_pss_bytes: (self.memory_pss / duration).round() as u64,
            memory_private_bytes: (self.memory_private / duration).round() as u64,
            memory_swap_bytes: (self.memory_swap / duration).round() as u64,
            memory_cgroup_bytes: (self.memory_cgroup / duration).round() as u64,
            process_count: (self.process_count / duration).round() as u64,
            thread_count: (self.thread_count / duration).round() as u64,
            major_faults_per_second: rounded(self.major_faults / duration, 2),
            gpu_percent: rounded(self.gpu / duration, 1),
            gpu_busy_percent: rounded(self.gpu_busy / duration, 1),
            gpu_memory_bytes: (self.gpu_memory / duration).round() as u64,
            gpu_memory_resident_bytes: (self.gpu_memory_resident / duration).round() as u64,
            gpu_memory_allocated_bytes: (self.gpu_memory_allocated / duration).round() as u64,
        }
    }
}

impl WeightedStorage {
    fn add(&mut self, duration_ms: u64, usage: &StorageUsage) {
        self.read_bytes = self.read_bytes.saturating_add(usage.disk_read_bytes);
        self.write_bytes = self.write_bytes.saturating_add(usage.disk_write_bytes);
        self.logical_read_bytes = self
            .logical_read_bytes
            .saturating_add(usage.logical_read_bytes);
        self.logical_write_bytes = self
            .logical_write_bytes
            .saturating_add(usage.logical_write_bytes);
        self.read_operations = self.read_operations.saturating_add(usage.read_operations);
        self.write_operations = self.write_operations.saturating_add(usage.write_operations);
        self.cancelled_write_bytes = self
            .cancelled_write_bytes
            .saturating_add(usage.cancelled_write_bytes);
        let weight = duration_ms as f64;
        self.open_file_bytes += usage.open_file_disk_bytes as f64 * weight;
        self.referenced_file_disk += usage.referenced_file_disk_bytes as f64 * weight;
        self.referenced_file_temporary += usage.referenced_file_temporary_bytes as f64 * weight;
        self.referenced_file_permanent += usage.referenced_file_permanent_bytes as f64 * weight;
        self.disk_space_total += usage.disk_space_total_bytes as f64 * weight;
        self.disk_space_temporary += usage.disk_space_temporary_bytes as f64 * weight;
        self.disk_space_permanent += usage.disk_space_permanent_bytes as f64 * weight;
    }

    fn finish(self, duration: f64) -> StorageUsage {
        StorageUsage {
            disk_read_bytes: self.read_bytes,
            disk_write_bytes: self.write_bytes,
            disk_read_bytes_per_second: rounded(self.read_bytes as f64 * 1000.0 / duration, 1),
            disk_write_bytes_per_second: rounded(self.write_bytes as f64 * 1000.0 / duration, 1),
            logical_read_bytes: self.logical_read_bytes,
            logical_write_bytes: self.logical_write_bytes,
            logical_read_bytes_per_second: rounded(
                self.logical_read_bytes as f64 * 1000.0 / duration,
                1,
            ),
            logical_write_bytes_per_second: rounded(
                self.logical_write_bytes as f64 * 1000.0 / duration,
                1,
            ),
            read_operations: self.read_operations,
            write_operations: self.write_operations,
            read_operations_per_second: rounded(self.read_operations as f64 * 1000.0 / duration, 1),
            write_operations_per_second: rounded(
                self.write_operations as f64 * 1000.0 / duration,
                1,
            ),
            cancelled_write_bytes: self.cancelled_write_bytes,
            open_file_disk_bytes: (self.open_file_bytes / duration).round() as u64,
            referenced_file_disk_bytes: (self.referenced_file_disk / duration).round() as u64,
            referenced_file_temporary_bytes: (self.referenced_file_temporary / duration).round()
                as u64,
            referenced_file_permanent_bytes: (self.referenced_file_permanent / duration).round()
                as u64,
            disk_space_total_bytes: (self.disk_space_total / duration).round() as u64,
            disk_space_temporary_bytes: (self.disk_space_temporary / duration).round() as u64,
            disk_space_permanent_bytes: (self.disk_space_permanent / duration).round() as u64,
        }
    }
}

impl WeightedNetwork {
    fn add(&mut self, duration_ms: u64, usage: &NetworkUsage) {
        self.receive_bytes = self
            .receive_bytes
            .saturating_add(usage.network_receive_bytes);
        self.transmit_bytes = self
            .transmit_bytes
            .saturating_add(usage.network_transmit_bytes);
        self.connection_count += usage.network_connection_count as f64 * duration_ms as f64;
    }

    fn finish(self, duration: f64) -> NetworkUsage {
        NetworkUsage {
            network_receive_bytes: self.receive_bytes,
            network_transmit_bytes: self.transmit_bytes,
            network_receive_bytes_per_second: rounded(
                self.receive_bytes as f64 * 1000.0 / duration,
                1,
            ),
            network_transmit_bytes_per_second: rounded(
                self.transmit_bytes as f64 * 1000.0 / duration,
                1,
            ),
            network_connection_count: (self.connection_count / duration).round() as u64,
        }
    }
}

fn merge_label(current: &mut String, next: &str) {
    if next.is_empty() || next == "unavailable" {
        return;
    }
    if current.is_empty() {
        current.push_str(next);
    } else if current != next {
        *current = "mixed".into();
    }
}

fn available_source(source: String) -> String {
    if source.is_empty() {
        "unavailable".into()
    } else {
        source
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
        let bucket_start = timestamp_ms - timestamp_ms % BUCKET_MILLISECONDS;
        let previous_bucket = self.pending.get(target_id).is_some_and(|pending| {
            pending.timestamp_ms != 0 && pending.timestamp_ms != bucket_start
        });
        if previous_bucket
            && let Some(point) = self
                .pending
                .remove(target_id)
                .and_then(PendingPoint::finish)
        {
            self.points
                .entry(target_id.to_owned())
                .or_default()
                .push_back(point);
        }
        let pending = self.pending.entry(target_id.to_owned()).or_default();
        pending.timestamp_ms = bucket_start;
        pending.add(duration_ms, usage);
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
        self.persist()
    }

    pub fn save_final(&mut self) -> std::io::Result<()> {
        self.flush_pending();
        self.persist()
    }

    fn persist(&mut self) -> std::io::Result<()> {
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
    use crate::model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage};

    #[test]
    fn ignores_unknown_and_future_history_formats() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("history.json");
        std::fs::write(&path, br#"{"version":99,"applications":{"app":[]}}"#)?;
        assert!(
            HistoryStore::load(Some(path.clone()))
                .query("app", None, 10)
                .0
                .is_empty()
        );
        std::fs::write(&path, b"not json")?;
        assert!(
            HistoryStore::load(Some(path))
                .query("app", None, 10)
                .0
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn aggregates_and_persists_resource_buckets() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("history.json");
        let mut store = HistoryStore::load(Some(path.clone()));
        let usage = ResourceUsage {
            compute: ComputeUsage {
                cpu_percent: 50.0,
                cpu_percent_of_machine: 12.5,
                memory_bytes: 1024,
                gpu_percent: 25.0,
                gpu_memory_bytes: 2048,
                ..ComputeUsage::default()
            },
            storage: StorageUsage {
                disk_read_bytes: 100,
                disk_write_bytes: 200,
                disk_read_bytes_per_second: 20.0,
                disk_write_bytes_per_second: 40.0,
                open_file_disk_bytes: 4096,
                ..StorageUsage::default()
            },
            energy: EnergyUsage {
                energy_mwh: 2.0,
                battery_percent: 0.004,
                power_watts: 3.6,
                estimated_app_power_watts: 3.6,
                battery_percent_per_hour: 7.2,
                energy_source: "rapl".into(),
                energy_confidence: "low".into(),
                ..EnergyUsage::default()
            },
            ..ResourceUsage::default()
        };
        let now = super::now_milliseconds();
        let bucket = now - now % super::BUCKET_MILLISECONDS;
        for timestamp in [bucket + 2_000, bucket + 7_000, bucket + 12_000] {
            store.record("example.desktop", timestamp, 5.0, &usage);
        }
        store.record(
            "example.desktop",
            bucket + super::BUCKET_MILLISECONDS + 2_000,
            2.0,
            &ResourceUsage::default(),
        );
        let (points, more) = store.query("example.desktop", None, 10);
        assert!(!more);
        assert_eq!(points.len(), 1);
        let point = &points[0].resources;
        assert_eq!(point.compute.cpu_percent, 50.0);
        assert_eq!(point.compute.gpu_percent, 25.0);
        assert_eq!(point.compute.gpu_memory_bytes, 2048);
        assert_eq!(point.storage.disk_read_bytes, 300);
        assert_eq!(point.storage.disk_write_bytes, 600);
        assert_eq!(point.storage.disk_read_bytes_per_second, 20.0);
        assert_eq!(point.storage.disk_write_bytes_per_second, 40.0);
        assert_eq!(point.storage.open_file_disk_bytes, 4096);
        assert_eq!(point.energy_mwh, 6.0);
        store.save()?;

        let loaded = HistoryStore::load(Some(path));
        assert_eq!(loaded.query("example.desktop", None, 10).0, points);
        Ok(())
    }
}
