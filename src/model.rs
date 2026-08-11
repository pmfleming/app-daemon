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
    pub cpu_percent: f64,
    pub memory_bytes: u64,
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
    pub cpu_percent: f64,
    pub memory_bytes: u64,
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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub id: String,
    pub action: String,
    pub target_id: String,
    pub status: String,
    pub message: String,
}
