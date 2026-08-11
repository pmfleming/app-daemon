use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    process::Stdio,
};

use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Default, Deserialize, Hash)]
pub struct Workspace {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Hash)]
pub struct Client {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub class: String,
    #[serde(rename = "initialClass", default)]
    pub initial_class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(rename = "focusHistoryID", default = "unfocused")]
    pub focus_rank: i64,
    #[serde(default = "mapped")]
    pub mapped: bool,
}

const fn unfocused() -> i64 {
    i64::MAX
}
const fn mapped() -> bool {
    true
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub available: bool,
    pub revision: u64,
    pub clients: Vec<Client>,
}

impl Snapshot {
    pub async fn load() -> Self {
        let output = match Command::new("hyprctl")
            .args(["clients", "-j"])
            .output()
            .await
        {
            Ok(value) if value.status.success() => value,
            _ => return Self::default(),
        };
        let Ok(mut clients) = serde_json::from_slice::<Vec<Client>>(&output.stdout) else {
            return Self::default();
        };
        clients.retain(|client| client.mapped && valid_address(&client.address));
        clients.sort_by_key(|client| client.focus_rank);
        let mut hasher = DefaultHasher::new();
        clients.hash(&mut hasher);
        Self {
            available: true,
            revision: hasher.finish(),
            clients,
        }
    }

    pub fn by_window_id(&self, id: &str) -> Option<&Client> {
        self.clients
            .iter()
            .find(|client| window_id(&client.address) == id)
    }
}

pub fn window_id(address: &str) -> String {
    format!(
        "window-{}",
        address.trim_start_matches("0x").to_ascii_lowercase()
    )
}

pub async fn focus(address: &str) -> anyhow::Result<()> {
    anyhow::ensure!(valid_address(address), "window address is invalid");
    let selector = format!("address:{address}");
    let lua = format!("hl.dsp.focus({{ window = '{selector}' }})");
    if dispatch(&["dispatch", &lua]).await {
        return Ok(());
    }
    if dispatch(&["dispatch", "focuswindow", &selector]).await {
        return Ok(());
    }
    anyhow::bail!("Hyprland rejected the focus request")
}

async fn dispatch(arguments: &[&str]) -> bool {
    let Ok(output) = Command::new("hyprctl")
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "ok"
}

fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|value| {
        !value.is_empty() && value.chars().all(|character| character.is_ascii_hexdigit())
    }) && address != "0x0"
}

#[cfg(test)]
mod tests {
    use super::window_id;

    #[test]
    fn creates_protocol_safe_window_ids() {
        assert_eq!(window_id("0xAb12"), "window-ab12");
    }
}
