use super::{ApplicationAction, ApplicationService, ExecuteParams};

#[tokio::test]
async fn query_waiting_for_settings_does_not_block_resource_publication() {
    use futures::{pin_mut, poll};

    let service = ApplicationService::build(false);
    let settings_update = service.settings.write().await;
    let query = service.query(serde_json::from_str("{}").unwrap());
    pin_mut!(query);
    assert!(poll!(&mut query).is_pending());

    // A query blocked on settings must not retain a resources guard. Otherwise
    // a queued sampler write and a revision reader holding settings can form
    // a cycle with the next settings update.
    let resource_publication = service
        .resources
        .try_write()
        .expect("query must acquire settings before resources, just like revision and execute");
    drop(resource_publication);
    drop(settings_update);
    tokio::time::timeout(std::time::Duration::from_secs(1), query)
        .await
        .expect("query should finish after the settings update");
}

#[test]
fn query_ranks_window_metadata_before_hydrating_only_selected_rows() {
    use super::query::{QueryParams, page};
    use crate::{
        catalog::Catalog, hyprland::Snapshot, resources::ResourceSnapshot, settings::SettingsStore,
    };
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("editor.desktop"),
        "[Desktop Entry]\nType=Application\nName=Editor\nExec=true\nCategories=Development;\nActions=New;\n[Desktop Action New]\nName=New document\nExec=true\n",
    )
    .unwrap();
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let windows: Vec<crate::hyprland::Client> = serde_json::from_value(serde_json::json!([
        {"address":"0x1", "pid":10, "title":"needle document", "class":"Editor", "focusHistoryID":0},
        {"address":"0x2", "pid":20, "title":"needle terminal", "class":"Terminal", "focusHistoryID":1}
    ])).unwrap();
    let snapshot = Snapshot {
        available: true,
        ..Default::default()
    };
    let settings = SettingsStore::load(None);
    let resources = ResourceSnapshot::default();
    let query = |text: &str, limit, category: &str| {
        page(
            &catalog,
            &snapshot,
            &resources,
            &settings,
            &QueryParams {
                query: text.into(),
                category: category.into(),
                generation: 42,
                limit,
            },
            std::collections::HashMap::from([
                ("editor.desktop".into(), vec![&windows[0]]),
                ("window-group:Terminal".into(), vec![&windows[1]]),
            ]),
            0,
        )
    };
    let full = query("needle", 10, "");
    let limited = query("needle", 1, "");
    assert_eq!(full.applications.len(), 2);
    assert!(limited.has_more);
    assert_eq!(limited.generation, 42);
    assert_eq!(
        serde_json::to_value(&limited.applications[0]).unwrap(),
        serde_json::to_value(&full.applications[0]).unwrap()
    );
    for (text, kind) in [
        (" needle ", "metadata"),
        ("nd", "acronym"),
        ("document desktop", "terms"),
    ] {
        let editor = &query(text, 10, "code").applications[0];
        assert_eq!(editor.match_kind, kind);
        assert_eq!(editor.desktop_actions, catalog.entries[0].actions);
        assert_eq!(editor.identity.id, "editor.desktop");
        assert!(editor.runtime.focused && editor.runtime.running);
        assert_eq!(editor.runtime.instances[0].title, "needle document");
        assert_eq!(editor.runtime.instances[0].id, "window-1");
    }
    assert_eq!(
        query("needle", 10, "shell").applications[0].identity.id,
        "window-group:Terminal"
    );
}

pub(super) fn close_missing(expected_revision: Option<u64>) -> ExecuteParams {
    ExecuteParams {
        target_id: "missing-window-group".into(),
        action: ApplicationAction::Close,
        window_id: None,
        desktop_action_id: None,
        expected_revision,
        workspace_id: None,
    }
}
