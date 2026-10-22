use crate::{
    metrics::{add_counter, available_label, merge_label, rounded},
    model::{
        ComputeUsage, HistoricalResourceUsage, NetworkUsage, ResourceAvailability,
        ResourceHistoryPoint, ResourcePeaks, ResourceUsage, StorageUsage,
    },
};

use super::BUCKET_MILLISECONDS;

#[derive(Debug, Default)]
pub(super) struct PendingPoint {
    pub(super) timestamp_ms: u64,
    pub(super) end_ms: u64,
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
    availability: Option<ResourceAvailability>,
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

pub(super) fn adjacent_in_bucket(
    first: &ResourceHistoryPoint,
    second: &ResourceHistoryPoint,
) -> bool {
    first.timestamp_ms == second.timestamp_ms.saturating_sub(second.duration_ms)
        && first.timestamp_ms.saturating_sub(1) / BUCKET_MILLISECONDS
            == second.timestamp_ms.saturating_sub(1) / BUCKET_MILLISECONDS
}

impl PendingPoint {
    pub(super) fn merge_finished(
        first: &ResourceHistoryPoint,
        second: &ResourceHistoryPoint,
    ) -> ResourceHistoryPoint {
        let mut merged = Self {
            end_ms: first.timestamp_ms.max(second.timestamp_ms),
            availability: first.resources.availability,
            ..Self::default()
        };
        match (&mut merged.availability, &second.resources.availability) {
            (Some(previous), Some(next)) => previous.intersect(next),
            _ => merged.availability = None,
        }
        for point in [first, second] {
            let resource = &point.resources;
            let duration = point.duration_ms;
            let weight = duration as f64;
            merged.duration_ms = merged.duration_ms.saturating_add(duration);
            merged.compute.add(duration, &resource.compute);
            merged
                .storage
                .add(duration, &resource.storage, |value| value);
            merged
                .network
                .add(duration, &resource.network, |value| value);
            merged.energy_mwh += resource.energy_mwh;
            merged.battery_percent += resource.battery_percent;
            merged.system_power += resource.system_power_watts * weight;
            merged.attributed_fraction += resource.attributed_fraction * weight;
            merged.coverage += resource.coverage * weight;
            merged.sample_count = merged.sample_count.saturating_add(resource.sample_count);
            merge_label(&mut merged.energy_source, &resource.energy_source);
            merge_label(&mut merged.energy_confidence, &resource.energy_confidence);
            merged.peaks.merge(&resource.peaks);
        }
        let start = first
            .timestamp_ms
            .saturating_sub(first.duration_ms)
            .min(second.timestamp_ms.saturating_sub(second.duration_ms));
        let mut point = merged.finish().unwrap_or_else(|| second.clone());
        // Equal-endpoint backfills can overlap. Do not turn duplicated weight
        // into an observation of time before either sample began.
        point.duration_ms = point.timestamp_ms.saturating_sub(start);
        point
    }

    pub(super) fn add(
        &mut self,
        usage: &ResourceUsage,
        range: std::ops::Range<u64>,
        sample_ms: u64,
    ) {
        let duration_ms = range.end - range.start;
        let fraction = duration_ms as f64 / sample_ms as f64;
        // Split totals, not gauges or rates. Cumulative integer allocation
        // preserves every byte/operation without cloning the resource payload.
        let portion = |total: u64| {
            let total = u128::from(total);
            (total * u128::from(range.end) / u128::from(sample_ms)
                - total * u128::from(range.start) / u128::from(sample_ms)) as u64
        };
        let available = ResourceAvailability::for_usage(usage);
        if let Some(previous) = &mut self.availability {
            previous.intersect(&available);
        } else {
            self.availability = Some(available);
        }
        self.duration_ms = self.duration_ms.saturating_add(duration_ms);
        self.end_ms = self.end_ms.saturating_add(duration_ms);
        self.compute.add(duration_ms, &usage.compute);
        self.storage.add(duration_ms, &usage.storage, portion);
        self.network.add(duration_ms, &usage.network, portion);
        self.energy_mwh += usage.energy.energy_mwh * fraction;
        self.battery_percent += usage.energy.battery_percent * fraction;
        let weight = duration_ms as f64;
        self.system_power += usage.energy.system_power_watts * weight;
        self.attributed_fraction += usage.energy.attributed_fraction * weight;
        self.coverage += usage.measurement.coverage * weight;
        self.sample_count = self.sample_count.saturating_add(1);
        merge_label(&mut self.energy_source, &usage.energy.energy_source);
        merge_label(&mut self.energy_confidence, &usage.energy.energy_confidence);
        self.peaks.merge(&ResourcePeaks {
            cpu_percent: usage.compute.cpu_percent,
            cpu_percent_of_machine: usage.compute.cpu_percent_of_machine,
            memory_bytes: usage.compute.memory_bytes,
            gpu_percent: usage.compute.gpu_percent,
            gpu_busy_percent: usage.compute.gpu_busy_percent,
            disk_read_bytes_per_second: usage.storage.disk_read_bytes_per_second,
            disk_write_bytes_per_second: usage.storage.disk_write_bytes_per_second,
            network_receive_bytes_per_second: usage.network.network_receive_bytes_per_second,
            network_transmit_bytes_per_second: usage.network.network_transmit_bytes_per_second,
            estimated_app_power_watts: usage.energy.estimated_app_power_watts,
        });
    }

    pub(super) fn finish(self) -> Option<ResourceHistoryPoint> {
        if self.duration_ms == 0 {
            return None;
        }
        let duration = self.duration_ms as f64;
        Some(ResourceHistoryPoint {
            timestamp_ms: self.end_ms,
            duration_ms: self.duration_ms,
            resources: HistoricalResourceUsage {
                availability: self.availability,
                metric_availability: crate::projection::MetricAvailability::empty(),
                compute: self.compute.finish(duration),
                storage: self.storage.finish(duration),
                network: self.network.finish(duration),
                energy_mwh: rounded(self.energy_mwh, 4),
                battery_percent: rounded(self.battery_percent, 6),
                average_power_watts: rounded(self.energy_mwh * 3_600.0 / duration, 3),
                system_power_watts: rounded(self.system_power / duration, 3),
                attributed_fraction: rounded(self.attributed_fraction / duration, 4),
                energy_source: available_label(self.energy_source),
                energy_confidence: available_label(self.energy_confidence),
                sample_count: self.sample_count,
                coverage: rounded(self.coverage / duration, 4),
                peaks: self.peaks,
            },
        })
    }
}

impl crate::model::ResourcePeaks {
    fn merge(&mut self, other: &Self) {
        self.cpu_percent = self.cpu_percent.max(other.cpu_percent);
        self.cpu_percent_of_machine = self
            .cpu_percent_of_machine
            .max(other.cpu_percent_of_machine);
        self.memory_bytes = self.memory_bytes.max(other.memory_bytes);
        self.gpu_percent = self.gpu_percent.max(other.gpu_percent);
        self.gpu_busy_percent = self.gpu_busy_percent.max(other.gpu_busy_percent);
        self.disk_read_bytes_per_second = self
            .disk_read_bytes_per_second
            .max(other.disk_read_bytes_per_second);
        self.disk_write_bytes_per_second = self
            .disk_write_bytes_per_second
            .max(other.disk_write_bytes_per_second);
        self.network_receive_bytes_per_second = self
            .network_receive_bytes_per_second
            .max(other.network_receive_bytes_per_second);
        self.network_transmit_bytes_per_second = self
            .network_transmit_bytes_per_second
            .max(other.network_transmit_bytes_per_second);
        self.estimated_app_power_watts = self
            .estimated_app_power_watts
            .max(other.estimated_app_power_watts);
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
            memory_bytes: average(self.memory, duration),
            memory_rss_bytes: average(self.memory_rss, duration),
            memory_pss_bytes: average(self.memory_pss, duration),
            memory_private_bytes: average(self.memory_private, duration),
            memory_swap_bytes: average(self.memory_swap, duration),
            memory_cgroup_bytes: average(self.memory_cgroup, duration),
            process_count: average(self.process_count, duration),
            thread_count: average(self.thread_count, duration),
            major_faults_per_second: rounded(self.major_faults / duration, 2),
            gpu_percent: rounded(self.gpu / duration, 1),
            gpu_busy_percent: rounded(self.gpu_busy / duration, 1),
            gpu_memory_bytes: average(self.gpu_memory, duration),
            gpu_memory_resident_bytes: average(self.gpu_memory_resident, duration),
            gpu_memory_allocated_bytes: average(self.gpu_memory_allocated, duration),
        }
    }
}

impl WeightedStorage {
    fn add(&mut self, duration_ms: u64, usage: &StorageUsage, portion: impl Fn(u64) -> u64) {
        add_counter(&mut self.read_bytes, portion(usage.disk_read_bytes));
        add_counter(&mut self.write_bytes, portion(usage.disk_write_bytes));
        add_counter(
            &mut self.logical_read_bytes,
            portion(usage.logical_read_bytes),
        );
        add_counter(
            &mut self.logical_write_bytes,
            portion(usage.logical_write_bytes),
        );
        add_counter(&mut self.read_operations, portion(usage.read_operations));
        add_counter(&mut self.write_operations, portion(usage.write_operations));
        add_counter(
            &mut self.cancelled_write_bytes,
            portion(usage.cancelled_write_bytes),
        );
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

            logical_read_bytes: self.logical_read_bytes,
            logical_write_bytes: self.logical_write_bytes,

            read_operations: self.read_operations,
            write_operations: self.write_operations,

            cancelled_write_bytes: self.cancelled_write_bytes,
            open_file_disk_bytes: average(self.open_file_bytes, duration),
            referenced_file_disk_bytes: average(self.referenced_file_disk, duration),
            referenced_file_temporary_bytes: average(self.referenced_file_temporary, duration),
            referenced_file_permanent_bytes: average(self.referenced_file_permanent, duration),
            disk_space_total_bytes: average(self.disk_space_total, duration),
            disk_space_temporary_bytes: average(self.disk_space_temporary, duration),
            disk_space_permanent_bytes: average(self.disk_space_permanent, duration),
            ..Default::default()
        }
        .with_rates(duration / 1_000.0)
    }
}

impl WeightedNetwork {
    fn add(&mut self, duration_ms: u64, usage: &NetworkUsage, portion: impl Fn(u64) -> u64) {
        add_counter(
            &mut self.receive_bytes,
            portion(usage.network_receive_bytes),
        );
        add_counter(
            &mut self.transmit_bytes,
            portion(usage.network_transmit_bytes),
        );
        self.connection_count += usage.network_connection_count as f64 * duration_ms as f64;
    }

    fn finish(self, duration: f64) -> NetworkUsage {
        NetworkUsage {
            network_receive_bytes: self.receive_bytes,
            network_transmit_bytes: self.transmit_bytes,
            network_connection_count: average(self.connection_count, duration),
            ..Default::default()
        }
        .with_rates(duration / 1_000.0)
    }
}

fn average(weighted: f64, duration: f64) -> u64 {
    (weighted / duration).round() as u64
}
