use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize, Hash)]
pub struct Workspace {
    #[serde(default)]
    pub id: Option<i64>,
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
        Self {
            available: true,
            revision: crate::metrics::revision(&clients),
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

pub(crate) fn window_event_relevant(event: &shelllist_hyprland::CompositorEvent) -> bool {
    use shelllist_hyprland::CompositorEvent::*;
    matches!(
        event,
        WindowOpened
            | WindowClosed
            | WindowMoved { .. }
            | ActiveWindowChanged
            | WindowTitleChanged
            | WorkspaceChanged
            | WorkspaceRenamed
            | WorkspaceMoved
            | WorkspaceIdChanged
            | ConfigReloaded
            | Malformed { .. }
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
    dispatch_with(&shelllist_hyprland::Client::default(), arguments).await
}

async fn dispatch_with(client: &shelllist_hyprland::Client, arguments: &[&str]) -> bool {
    // A stale session must not redirect window mutations to another compositor.
    client
        .request_exact(&arguments.join(" "))
        .await
        .is_ok_and(|response| shelllist_hyprland::ack(&response).is_ok())
}

fn valid_address(address: &str) -> bool {
    shelllist_hyprland::WindowAddress::parse(address).is_ok()
}

#[cfg(test)]
mod tests {
    use super::Snapshot;

    #[test]
    fn special_workspace_without_number_keeps_the_application_available() {
        let snapshot = Snapshot::from_response(
            r#"[
            {"address":"0x123","pid":42,"class":"app","workspace":{"address":"special:probe","type":"special","name":"special:probe"}},
            {"address":"0x456","pid":43,"class":"app","workspace":{"id":3,"name":"3"}}
        ]"#,
        );
        assert!(snapshot.available);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.clients[0].workspace.id, None);
        assert_eq!(snapshot.clients[0].workspace.name, "special:probe");
        assert_eq!(snapshot.clients[1].workspace.id, Some(3));
    }
}
