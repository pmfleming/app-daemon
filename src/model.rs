use serde::{Deserialize, Serialize};

macro_rules! usage_fields {
    ($name:ident { $($(#[$meta:meta])* $field:ident: $type:ty),+ $(,)? }) => {
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name { $( $(#[$meta])* pub $field: $type, )+ }
    };
}

usage_fields!(ComputeUsage {
    /// Top-compatible CPU usage: 100% is one fully occupied logical CPU.
    cpu_percent: f64,
    /// CPU usage as a percentage of the whole machine, always capped at 100%.
    cpu_percent_of_machine: f64,
    /// Best available physical-memory estimate: PSS when readable, RSS otherwise.
    memory_bytes: u64,
    memory_rss_bytes: u64,
    memory_pss_bytes: u64,
    memory_private_bytes: u64,
    memory_swap_bytes: u64,
    memory_cgroup_bytes: u64,
    process_count: u64,
    thread_count: u64,
    major_faults_per_second: f64,
    /// Aggregate DRM engine occupancy; it can exceed 100% across engines.
    gpu_percent: f64,
    /// Occupancy of the busiest DRM engine, capped at 100%.
    gpu_busy_percent: f64,
    /// Resident GPU memory reported by DRM, falling back to allocated memory.
    gpu_memory_bytes: u64,
    gpu_memory_resident_bytes: u64,
    gpu_memory_allocated_bytes: u64,
});
usage_fields!(StorageUsage {
    /// Physical storage bytes completed during the current interval.
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    disk_read_bytes_per_second: f64,
    disk_write_bytes_per_second: f64,
    /// Logical process I/O, including page-cache hits.
    logical_read_bytes: u64,
    logical_write_bytes: u64,
    logical_read_bytes_per_second: f64,
    logical_write_bytes_per_second: f64,
    read_operations: u64,
    write_operations: u64,
    read_operations_per_second: f64,
    write_operations_per_second: f64,
    cancelled_write_bytes: u64,
    /// Allocated size of unique regular files currently held open.
    open_file_disk_bytes: u64,
    /// Allocated size of unique open or mapped regular files.
    referenced_file_disk_bytes: u64,
    referenced_file_temporary_bytes: u64,
    referenced_file_permanent_bytes: u64,
    /// Allocated size of identified application-owned data directories.
    disk_space_total_bytes: u64,
    disk_space_temporary_bytes: u64,
    disk_space_permanent_bytes: u64,
});
usage_fields!(NetworkUsage {
    network_receive_bytes: u64,
    network_transmit_bytes: u64,
    network_receive_bytes_per_second: f64,
    network_transmit_bytes_per_second: f64,
    network_connection_count: u64,
});
usage_fields!(EnergyUsage {
    /// Application-attributed energy. This is only populated for attributable domains.
    energy_mwh: f64,
    battery_percent: f64,
    /// Estimated application power, retained under the v1-compatible field name.
    power_watts: f64,
    estimated_app_power_watts: f64,
    system_power_watts: f64,
    battery_percent_per_hour: f64,
    attributed_fraction: f64,
    energy_source: String,
    energy_confidence: String,
});

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceMeasurement {
    pub sample_interval_ms: u64,
    pub attribution_method: String,
    pub coverage: f64,
    pub memory_source: String,
    pub gpu_available: bool,
    pub storage_available: bool,
    pub disk_space_scope: String,
    pub network_available: bool,
    pub network_bytes_available: bool,
    pub network_connections_available: bool,
    pub resources_shared: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceUsage {
    #[serde(flatten)]
    pub compute: ComputeUsage,
    #[serde(flatten)]
    pub storage: StorageUsage,
    #[serde(flatten)]
    pub network: NetworkUsage,
    #[serde(flatten)]
    pub energy: EnergyUsage,
    pub measurement: ResourceMeasurement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopActionSummary {
    pub id: String,
    pub name: String,
    pub icon: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub id: String,
    pub title: String,
    pub class: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub focused: bool,
    pub focus_rank: i64,
    #[serde(flatten)]
    pub resources: ResourceUsage,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationIdentity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub icon: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub startup_class: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationRuntime {
    pub running: bool,
    pub focused: bool,
    pub running_count: usize,
    #[serde(flatten)]
    pub resources: ResourceUsage,
    pub instances: Vec<WindowSummary>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationSummary {
    #[serde(flatten)]
    pub identity: ApplicationIdentity,
    pub revision: u64,
    #[serde(flatten)]
    pub runtime: ApplicationRuntime,
    pub desktop_actions: Vec<DesktopActionSummary>,
    pub score: i64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationPage {
    pub revision: u64,
    pub generation: u64,
    pub applications: Vec<ApplicationSummary>,
    pub has_more: bool,
    pub hyprland_available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcePeaks {
    pub cpu_percent: f64,
    pub cpu_percent_of_machine: f64,
    pub memory_bytes: u64,
    pub gpu_percent: f64,
    pub gpu_busy_percent: f64,
    pub disk_read_bytes_per_second: f64,
    pub disk_write_bytes_per_second: f64,
    pub estimated_app_power_watts: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoricalResourceUsage {
    #[serde(flatten)]
    pub compute: ComputeUsage,
    #[serde(flatten)]
    pub storage: StorageUsage,
    #[serde(flatten)]
    pub network: NetworkUsage,
    pub energy_mwh: f64,
    pub battery_percent: f64,
    pub average_power_watts: f64,
    pub system_power_watts: f64,
    pub attributed_fraction: f64,
    pub energy_source: String,
    pub energy_confidence: String,
    pub sample_count: u64,
    pub coverage: f64,
    pub peaks: ResourcePeaks,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceHistoryPoint {
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    #[serde(flatten)]
    pub resources: HistoricalResourceUsage,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationResourceHistory {
    pub target_id: String,
    /// Chronological page ordered from oldest to newest.
    pub points: Vec<ResourceHistoryPoint>,
    pub has_more: bool,
    /// Opaque forward-pagination cursor. Pass it back as `cursor` to fetch the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub id: String,
    pub action: String,
    pub target_id: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_scope: Option<String>,
}
