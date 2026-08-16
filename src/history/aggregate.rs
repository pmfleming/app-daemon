use crate::{
    metrics::rounded,
    model::{
        ComputeUsage, HistoricalResourceUsage, NetworkUsage, ResourceHistoryPoint, ResourcePeaks,
        ResourceUsage, StorageUsage,
    },
};

use super::BUCKET_MILLISECONDS;

#[derive(Debug, Default)]
pub(super) struct PendingPoint {
    pub(super) timestamp_ms: u64,
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
    pub(super) fn add(&mut self, duration_ms: u64, usage: &ResourceUsage) {
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

    pub(super) fn finish(self) -> Option<ResourceHistoryPoint> {
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
        add_counter(&mut self.read_bytes, usage.disk_read_bytes);
        add_counter(&mut self.write_bytes, usage.disk_write_bytes);
        add_counter(&mut self.logical_read_bytes, usage.logical_read_bytes);
        add_counter(&mut self.logical_write_bytes, usage.logical_write_bytes);
        add_counter(&mut self.read_operations, usage.read_operations);
        add_counter(&mut self.write_operations, usage.write_operations);
        add_counter(&mut self.cancelled_write_bytes, usage.cancelled_write_bytes);
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
            disk_read_bytes_per_second: per_second(self.read_bytes, duration),
            disk_write_bytes_per_second: per_second(self.write_bytes, duration),
            logical_read_bytes: self.logical_read_bytes,
            logical_write_bytes: self.logical_write_bytes,
            logical_read_bytes_per_second: per_second(self.logical_read_bytes, duration),
            logical_write_bytes_per_second: per_second(self.logical_write_bytes, duration),
            read_operations: self.read_operations,
            write_operations: self.write_operations,
            read_operations_per_second: per_second(self.read_operations, duration),
            write_operations_per_second: per_second(self.write_operations, duration),
            cancelled_write_bytes: self.cancelled_write_bytes,
            open_file_disk_bytes: average(self.open_file_bytes, duration),
            referenced_file_disk_bytes: average(self.referenced_file_disk, duration),
            referenced_file_temporary_bytes: average(self.referenced_file_temporary, duration),
            referenced_file_permanent_bytes: average(self.referenced_file_permanent, duration),
            disk_space_total_bytes: average(self.disk_space_total, duration),
            disk_space_temporary_bytes: average(self.disk_space_temporary, duration),
            disk_space_permanent_bytes: average(self.disk_space_permanent, duration),
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
            network_receive_bytes_per_second: per_second(self.receive_bytes, duration),
            network_transmit_bytes_per_second: per_second(self.transmit_bytes, duration),
            network_connection_count: (self.connection_count / duration).round() as u64,
        }
    }
}

fn average(weighted: f64, duration: f64) -> u64 {
    (weighted / duration).round() as u64
}

fn per_second(total: u64, duration_ms: f64) -> f64 {
    rounded(total as f64 * 1_000.0 / duration_ms, 1)
}

fn add_counter(counter: &mut u64, value: u64) {
    *counter = counter.saturating_add(value);
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
