use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default, Deserialize, Hash)]
pub struct Workspace {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Hash)]
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
    pub pid: u32,
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

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub available: bool,
    pub revision: u64,
    pub clients: Vec<Client>,
}

impl Snapshot {
    pub async fn load() -> Self {
        let Ok(output) = shelllist_hyprland::Client::default()
            .request("j/clients")
            .await
        else {
            return Self::default();
        };
        Self::from_response(&output)
    }

    fn from_response(output: &str) -> Self {
        let Ok(mut clients) = serde_json::from_str::<Vec<Client>>(output) else {
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

pub(crate) async fn watch_window_events(sender: mpsc::Sender<shelllist_hyprland::Event>) {
    shelllist_hyprland::watch_events_detailed(sender).await;
}

pub(crate) fn window_event_relevant(line: &str) -> bool {
    let Some((event, _)) = line.split_once(">>") else {
        return false;
    };
    matches!(
        event,
        "openwindow"
            | "closewindow"
            | "movewindow"
            | "movewindowv2"
            | "activewindow"
            | "activewindowv2"
            | "windowtitle"
            | "windowtitlev2"
            | "workspace"
            | "workspacev2"
            | "renameworkspace"
            | "moveworkspace"
            | "moveworkspacev2"
            | "configreloaded"
    )
}

pub async fn focus(address: &str) -> anyhow::Result<()> {
    let selector = address_selector(address)?;
    let lua = format!("hl.dsp.focus({{ window = '{selector}' }})");
    dispatch_window(&lua, "focuswindow", &selector, "focus").await
}

pub async fn close(address: &str) -> anyhow::Result<()> {
    let selector = address_selector(address)?;
    let lua = format!("hl.dsp.window.close({{ window = '{selector}' }})");
    dispatch_window(&lua, "closewindow", &selector, "close").await
}

async fn dispatch_window(
    lua: &str,
    legacy_command: &str,
    selector: &str,
    operation: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        dispatch(&["dispatch", lua]).await
            || dispatch(&["dispatch", legacy_command, selector]).await,
        "Hyprland rejected the {operation} request"
    );
    Ok(())
}

pub async fn move_to_workspace(address: &str, workspace: &str) -> anyhow::Result<()> {
    let selector = address_selector(address)?;
    let workspace = workspace_selector(workspace)?;
    let lua = format!(
        "hl.dsp.window.move({{ workspace = '{workspace}', follow = false, window = '{selector}' }})"
    );
    if dispatch(&["dispatch", &lua]).await {
        return Ok(());
    }
    let argument = format!("{workspace},{selector}");
    anyhow::ensure!(
        dispatch(&["dispatch", "movetoworkspacesilent", &argument]).await,
        "Hyprland rejected the workspace move request"
    );
    Ok(())
}

fn address_selector(address: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_address(address), "window address is invalid");
    Ok(format!("address:{address}"))
}

fn workspace_selector(workspace: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !workspace.is_empty()
            && workspace
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_-.+:".contains(character)),
        "workspace is invalid"
    );
    Ok(workspace)
}

async fn dispatch(arguments: &[&str]) -> bool {
    shelllist_hyprland::Client::default()
        .request(&arguments.join(" "))
        .await
        .is_ok_and(|response| response.trim() == "ok")
}

fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|value| {
        !value.is_empty() && value.chars().all(|character| character.is_ascii_hexdigit())
    }) && address != "0x0"
}

#[cfg(test)]
mod tests {
    use super::{address_selector, workspace_selector};

    #[test]
    fn filters_unrelated_events_but_retains_client_and_focus_changes() {
        for event in [
            "openwindow",
            "closewindow",
            "movewindowv2",
            "windowtitlev2",
            "activewindowv2",
            "workspacev2",
            "renameworkspace",
            "configreloaded",
        ] {
            assert!(
                super::window_event_relevant(&format!("{event}>>data")),
                "{event}"
            );
        }
        for event in [
            "openlayer>>bar",
            "closelayer>>osd",
            "submap>>resize",
            "activelayout>>kbd,us",
            "malformed",
        ] {
            assert!(!super::window_event_relevant(event), "{event}");
        }
    }

    #[test]
    fn parses_direct_ipc_snapshot_and_rejects_partial_or_invalid_data() {
        let state = super::Snapshot::from_response(
            r#"[{"address":"0x123","mapped":true,"class":"app","pid":42},{"address":"0x0"}]"#,
        );
        assert!(state.available);
        assert_eq!(state.clients.len(), 1);
        assert_eq!(state.clients[0].pid, 42);
        assert!(!super::Snapshot::from_response("truncated json").available);
        assert!(super::Snapshot::from_response("[]").available);
    }

    #[test]
    fn validates_window_selectors_for_dispatch() -> anyhow::Result<()> {
        assert_eq!(address_selector("0xAb12")?, "address:0xAb12");
        assert!(address_selector("not-an-address").is_err());
        assert_eq!(
            workspace_selector("special:scratchpad")?,
            "special:scratchpad"
        );
        assert!(workspace_selector("2,address:0x1").is_err());
        Ok(())
    }
}
