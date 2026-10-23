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
        .expect("query should finish after the settings update")
        .expect("query projection should succeed");
}

async fn gated_blocking_worker() -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let job = tokio::task::spawn_blocking(move || {
        let _ = entered.send(());
        let _ = wait.recv_timeout(std::time::Duration::from_secs(3));
    });
    started.await.unwrap();
    (release, job)
}

#[test]
fn query_uses_immutable_snapshots_without_blocking_state_publication() {
    use futures::poll;
    use std::sync::Arc;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let service = ApplicationService::build(false);
            let (release, gate) = gated_blocking_worker().await;
            let mut query = Box::pin(service.query(serde_json::from_str("{}").unwrap()));
            assert!(
                poll!(query.as_mut()).is_pending(),
                "projection must yield to the blocking worker"
            );
            assert!(
                service.resources.try_write().is_ok(),
                "queued projection holds no resources lock"
            );
            assert!(
                service.settings.try_write().is_ok(),
                "queued projection holds no settings lock"
            );
            *service.windows.write().await = Arc::new(crate::hyprland::Snapshot {
                available: true,
                ..Default::default()
            });
            // A control/read operation can still observe state while the worker is held.
            service.revision().await;
            release.send(()).unwrap();
            gate.await.unwrap();
            let page = query.await.unwrap();
            assert!(
                !page.hyprland_available,
                "publication cannot mutate the query's captured snapshot"
            );
            assert!(
                service
                    .query(serde_json::from_str("{}").unwrap())
                    .await
                    .unwrap()
                    .hyprland_available
            );
        });
}

#[test]
fn abandoned_queries_keep_blocking_admission_until_the_jobs_finish() {
    use futures::poll;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let service = ApplicationService::build(false);
            let (release, gate) = gated_blocking_worker().await;
            let mut first = Box::pin(service.query(serde_json::from_str("{}").unwrap()));
            let mut second = Box::pin(service.query(serde_json::from_str("{}").unwrap()));
            assert!(poll!(first.as_mut()).is_pending());
            assert!(poll!(second.as_mut()).is_pending());
            drop(first);
            drop(second);
            assert_eq!(
                service.query_slots.available_permits(),
                0,
                "dropping read waiters must not admit unbounded detached blocking work"
            );
            let mut next = Box::pin(service.query(serde_json::from_str("{}").unwrap()));
            assert!(poll!(next.as_mut()).is_pending());
            assert!(service.resources.try_write().is_ok());
            release.send(()).unwrap();
            gate.await.unwrap();
            next.await.unwrap();
            assert_eq!(service.query_slots.available_permits(), 2);
        });
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
        "[Desktop Entry]\nType=Application\nName=Editor\nGenericName=Document editor\nComment=Edit café\nIcon=editor-icon\nKeywords=Writer;Note;\nStartupWMClass=EditWindow\nExec=true\nCategories=Development;\nActions=New;\n[Desktop Action New]\nName=New document\nExec=true\n",
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
    let (settings, _) = SettingsStore::load(None)
        .prepare_update("editor.desktop".into(), "code".into())
        .unwrap();
    let (settings, _) = settings
        .prepare_update("window-group:Terminal".into(), "browser".into())
        .unwrap();
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
        ("CAFÉ", "metadata"),
        ("Editor", "exact-name"),
        ("editor.desktop", "exact-id"),
        ("wn", "acronym"),
        ("document desktop", "terms"),
    ] {
        let editor = &query(text, 10, "code").applications[0];
        assert_eq!(editor.match_kind, kind);
        assert_eq!(editor.desktop_actions, catalog.entries[0].actions);
        assert_eq!(
            serde_json::to_value(&editor.identity).unwrap(),
            serde_json::json!({
                "id":"editor.desktop", "kind":"desktop-application", "name":"Editor",
                "generic_name":"Document editor", "comment":"Edit café", "icon":"editor-icon",
                "keywords":["Writer","Note"], "categories":["Development"], "category":"code",
                "default_workspace_id":"3", "startup_class":"EditWindow"
            })
        );
        assert_eq!(
            editor.revision,
            super::query::target_revision(
                "editor.desktop",
                catalog.by_id("editor.desktop"),
                &[&windows[0]],
                settings.for_application("editor.desktop"),
                true,
            )
        );
        assert!(editor.runtime.focused && editor.runtime.running);
        assert_eq!(editor.runtime.instances[0].title, "needle document");
        assert_eq!(editor.runtime.instances[0].id, "window-1");
    }
    let unmatched = &query("needle", 10, "shell").applications[0];
    assert_eq!(unmatched.identity.id, "window-group:Terminal");
    assert_eq!(unmatched.identity.keywords, ["needle terminal", "Terminal"]);
    assert_eq!(unmatched.identity.default_workspace_id, None);
    assert!(unmatched.desktop_actions.is_empty());
    assert_eq!(
        unmatched.revision,
        super::query::target_revision(
            &unmatched.identity.id,
            None,
            &[&windows[1]],
            settings.for_application(&unmatched.identity.id),
            true,
        )
    );
    let corpus = "Terminal Running window  window-group:Terminal  needle terminal Terminal needle terminal Terminal".to_lowercase();
    assert_eq!(
        unmatched.match_score,
        7_500 - corpus.find("needle").unwrap() as i64
    );
    assert!(query("no-such-match", 10, "").applications.is_empty());
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
