use serde::{Deserialize, Serialize};

/// CPU, memory, and GPU observations for the current interval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputeUsage {
    /// Top-compatible CPU usage: 100% is one fully occupied logical CPU.
    pub cpu_percent: f64,
    /// CPU usage as a percentage of the whole machine, always capped at 100%.
    pub cpu_percent_of_machine: f64,
    /// Best available physical-memory estimate: PSS when readable, RSS otherwise.
    pub memory_bytes: u64,
    pub memory_rss_bytes: u64,
    pub memory_pss_bytes: u64,
    pub memory_private_bytes: u64,
    pub memory_swap_bytes: u64,
    pub memory_cgroup_bytes: u64,
    pub process_count: u64,
    pub thread_count: u64,
    pub major_faults_per_second: f64,
    /// Aggregate DRM engine occupancy; it can exceed 100% across engines.
    pub gpu_percent: f64,
    /// Occupancy of the busiest DRM engine, capped at 100%.
    pub gpu_busy_percent: f64,
    /// Resident GPU memory reported by DRM, falling back to allocated memory.
    pub gpu_memory_bytes: u64,
    pub gpu_memory_resident_bytes: u64,
    pub gpu_memory_allocated_bytes: u64,
}

/// Interval I/O and allocated storage footprints, in bytes unless named otherwise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageUsage {
    /// Physical storage bytes completed during the current interval.
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_read_bytes_per_second: f64,
    pub disk_write_bytes_per_second: f64,
    /// Logical process I/O, including page-cache hits.
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub logical_read_bytes_per_second: f64,
    pub logical_write_bytes_per_second: f64,
    pub read_operations: u64,
    pub write_operations: u64,
    pub read_operations_per_second: f64,
    pub write_operations_per_second: f64,
    pub cancelled_write_bytes: u64,
    /// Allocated size of unique regular files currently held open.
    pub open_file_disk_bytes: u64,
    /// Allocated size of unique open or mapped regular files.
    pub referenced_file_disk_bytes: u64,
    pub referenced_file_temporary_bytes: u64,
    pub referenced_file_permanent_bytes: u64,
    /// Allocated size of identified application-owned data directories.
    pub disk_space_total_bytes: u64,
    pub disk_space_temporary_bytes: u64,
    pub disk_space_permanent_bytes: u64,
}

/// Socket-attributed byte deltas, rates, and connection count.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkUsage {
    pub network_receive_bytes: u64,
    pub network_transmit_bytes: u64,
    pub network_receive_bytes_per_second: f64,
    pub network_transmit_bytes_per_second: f64,
    pub network_connection_count: u64,
}

/// Estimated energy attribution together with its source and confidence labels.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnergyUsage {
    /// Application-attributed energy. This is only populated for attributable domains.
    pub energy_mwh: f64,
    pub battery_percent: f64,
    /// Estimated application power, retained under the v1-compatible field name.
    pub power_watts: f64,
    pub estimated_app_power_watts: f64,
    pub system_power_watts: f64,
    pub battery_percent_per_hour: f64,
    pub attributed_fraction: f64,
    pub energy_source: String,
    pub energy_confidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceMeasurement {
    pub sample_interval_ms: u64,
    pub attribution_method: String,
    pub coverage: f64,
    pub memory_source: String,
    pub gpu_available: bool,
    pub storage_available: bool,
    pub referenced_files_available: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// One of Shelllist's five launcher categories.
    pub category: String,
    /// Preferred Hyprland workspace for newly launched windows.
    pub default_workspace_id: Option<String>,
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
    /// Textual relevance for the active query.
    pub match_score: i64,
    /// The strongest textual match tier, such as `exact-name` or `acronym`.
    pub match_kind: String,
    /// Focus/recency score independent of textual relevance.
    pub runtime_score: i64,
    /// Combined sort score retained for clients that consume a single value.
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
    pub network_receive_bytes_per_second: f64,
    pub network_transmit_bytes_per_second: f64,
}

/// A capability is true only when available throughout the observed bucket.
/// Missing metadata in older history files means unknown, not measured zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceAvailability {
    pub cpu: bool,
    pub memory: bool,
    pub gpu: bool,
    pub storage: bool,
    pub referenced_files: bool,
    pub disk_space: bool,
    pub network_bytes: bool,
    pub network_connections: bool,
    pub energy: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoricalResourceUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<ResourceAvailability>,
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
    pub summary: crate::history::summary::HistorySummary,
    /// Chronological page ordered from oldest to newest.
    pub points: Vec<ResourceHistoryPoint>,
    pub has_more: bool,
    /// Opaque forward-pagination cursor. Pass it back as `cursor` to fetch the next page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplicationEnergySummary {
    pub target_id: String,
    pub name: String,
    pub icon: String,
    pub energy_mwh: f64,
    pub share: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplicationEnergyOverview {
    pub since_ms: u64,
    pub until_ms: u64,
    pub total_energy_mwh: f64,
    pub energy_source: String,
    pub energy_confidence: String,
    pub applications: Vec<ApplicationEnergySummary>,
}

/// Launch handoff and workspace placement have independent outcomes. A
/// completed launch with unavailable/failed placement must never be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePlacement {
    pub workspace_id: String,
    pub status: PlacementStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlacementStatus {
    Pending,
    Placed,
    Unavailable,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<WorkspacePlacement>,
}
