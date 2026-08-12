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
    memory_bytes: u64,
    /// DRM engine time: 100% is one fully occupied GPU engine.
    gpu_percent: f64,
    /// Resident GPU memory reported by DRM, falling back to allocated memory.
    gpu_memory_bytes: u64,
});
usage_fields!(StorageUsage {
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    disk_read_bytes_per_second: f64,
    disk_write_bytes_per_second: f64,
    open_file_disk_bytes: u64,
});
usage_fields!(EnergyUsage {
    energy_mwh: f64,
    battery_percent: f64,
    power_watts: f64,
    battery_percent_per_hour: f64,
    energy_source: String,
});

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceUsage {
    #[serde(flatten)]
    pub compute: ComputeUsage,
    #[serde(flatten)]
    pub storage: StorageUsage,
    #[serde(flatten)]
    pub energy: EnergyUsage,
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
pub struct ApplicationSummary {
    pub id: String,
    pub revision: u64,
    pub kind: String,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub icon: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub startup_class: String,
    pub running: bool,
    pub focused: bool,
    pub running_count: usize,
    #[serde(flatten)]
    pub resources: ResourceUsage,
    pub instances: Vec<WindowSummary>,
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
pub struct HistoricalResourceUsage {
    #[serde(flatten)]
    pub compute: ComputeUsage,
    #[serde(flatten)]
    pub storage: StorageUsage,
    pub energy_mwh: f64,
    pub battery_percent: f64,
    pub average_power_watts: f64,
    pub energy_source: String,
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
    pub points: Vec<ResourceHistoryPoint>,
    pub has_more: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub id: String,
    pub action: String,
    pub target_id: String,
    pub status: String,
    pub message: String,
}
