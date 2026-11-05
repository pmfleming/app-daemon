pub use shelllist_hyprland::workspace::{Window as Client, WorkspaceRef as Workspace};

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
    shelllist_hyprland::execute(
        &shelllist_hyprland::Client::default(),
        &shelllist_hyprland::lua::focus_window(address)?,
    )
    .await
}
pub async fn close(address: &str) -> anyhow::Result<()> {
    shelllist_hyprland::execute(
        &shelllist_hyprland::Client::default(),
        &shelllist_hyprland::lua::close_window(address)?,
    )
    .await
}
pub async fn move_to_workspace(address: &str, workspace: &str) -> anyhow::Result<()> {
    shelllist_hyprland::execute(
        &shelllist_hyprland::Client::default(),
        &shelllist_hyprland::lua::move_window(address, workspace)?,
    )
    .await
}

fn valid_address(address: &str) -> bool {
    shelllist_hyprland::WindowAddress::parse(address).is_ok()
}

#[cfg(test)]
mod tests {
    use super::Snapshot;

    #[test]
    fn shared_models_keep_ranking_filter_bad_addresses_and_reject_missing_identity() {
        let snapshot = Snapshot::from_response(
            r#"[
            {"address":"not-a-selector","pid":1,"workspace":{"id":1}},
            {"address":"0x123","pid":2,"workspace":{"id":1}},
            {"address":"0x456","pid":3,"workspace":{"id":1},"focusHistoryID":0},
            {"address":"0x789","pid":4,"workspace":{"id":1},"mapped":false}
        ]"#,
        );
        assert!(snapshot.available);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.clients[0].address, "0x456");
        assert_eq!(snapshot.clients[1].focus_rank, i64::MAX);
        assert!(
            !Snapshot::from_response(r#"[{"address":"0x123","workspace":{"id":1}}]"#).available
        );
    }

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
