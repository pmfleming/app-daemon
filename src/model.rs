use serde::{Deserialize, Serialize};

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
    /// Top-compatible CPU usage; 100% represents one logical CPU.
    pub cpu_percent: f64,
    pub cpu_percent_of_machine: f64,
    pub memory_bytes: u64,
    pub gpu_percent: f64,
    pub gpu_memory_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_read_bytes_per_second: f64,
    pub disk_write_bytes_per_second: f64,
    pub open_file_disk_bytes: u64,
    pub energy_mwh: f64,
    pub battery_percent: f64,
    pub power_watts: f64,
    pub battery_percent_per_hour: f64,
    pub energy_source: String,
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
    /// Top-compatible CPU usage; 100% represents one logical CPU.
    pub cpu_percent: f64,
    pub cpu_percent_of_machine: f64,
    pub memory_bytes: u64,
    pub gpu_percent: f64,
    pub gpu_memory_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_read_bytes_per_second: f64,
    pub disk_write_bytes_per_second: f64,
    pub open_file_disk_bytes: u64,
    pub energy_mwh: f64,
    pub battery_percent: f64,
    pub power_watts: f64,
    pub battery_percent_per_hour: f64,
    pub energy_source: String,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceHistoryPoint {
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub cpu_percent: f64,
    pub cpu_percent_of_machine: f64,
    pub memory_bytes: u64,
    #[serde(default)]
    pub gpu_percent: f64,
    #[serde(default)]
    pub gpu_memory_bytes: u64,
    #[serde(default)]
    pub disk_read_bytes: u64,
    #[serde(default)]
    pub disk_write_bytes: u64,
    #[serde(default)]
    pub disk_read_bytes_per_second: f64,
    #[serde(default)]
    pub disk_write_bytes_per_second: f64,
    #[serde(default)]
    pub open_file_disk_bytes: u64,
    pub energy_mwh: f64,
    pub battery_percent: f64,
    pub average_power_watts: f64,
    pub energy_source: String,
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
