use super::*;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, Catalog, Observations) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("editor.desktop"),
        "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor\nCategories=Development;\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("untracked.desktop"),
        "[Desktop Entry]\nType=Application\nName=Untracked\nExec=untracked\n",
    )
    .unwrap();
    let catalog = Catalog::from_paths(vec![root.path().to_owned()]);
    let observations = Observations::decode(&serde_json::to_vec(&json!({"schema_version":1,
        "applications":{"available":true,"total":3,"packages":[
            {"scope":"system","name":"editor-package","version":"1.0","desktop_ids":["editor.desktop"],"vendor_tool":"codex"},
            {"scope":"user:test","name":"editor-package","version":"0.9","desktop_ids":["editor.desktop"]},
            {"scope":"system","name":"Untracked","version":"3.0","kind":"command-line"}
        ]}, "tools":[{"tool":"codex","installed_version":"2.0","latest_version":"2.1",
            "latest_release":{"source":"https://example.test/releases/2.1"},"last_successful_check":100,"last_attempt":101,"stale":false}]
    })).unwrap()).unwrap();
    (root, catalog, observations)
}

#[test]
fn explicit_identity_preserves_scopes_and_active_versions_without_name_guessing() {
    let (_root, catalog, observations) = fixture();
    let snapshot = compose(&catalog, &SettingsStore::default(), &observations);
    assert_eq!(snapshot.entries.len(), 3);
    let editor = snapshot
        .entries
        .iter()
        .find(|r| r.id == "editor.desktop")
        .unwrap();
    assert_eq!(editor.name, "Editor");
    assert_eq!(editor.category, "code");
    assert_eq!(editor.installations.len(), 2);
    assert_eq!(
        editor.installations[0].declared_version.as_deref(),
        Some("1.0")
    );
    assert_eq!(
        editor.installations[0].active_version.as_deref(),
        Some("2.0")
    );
    assert_eq!(
        editor.installations[0]
            .release
            .as_ref()
            .unwrap()
            .version
            .as_deref(),
        Some("2.1")
    );
    assert!(
        snapshot
            .entries
            .iter()
            .find(|r| r.id == "untracked.desktop")
            .unwrap()
            .installations
            .is_empty()
    );
    assert_eq!(snapshot.entries.last().unwrap().kind, "command-line");
}

#[test]
fn turso_roundtrip_preferences_removals_and_pristine_identity() {
    let (root, catalog, observations) = fixture();
    let path = root.path().join("catalog/catalog.db");
    // Old JSON is deliberately not imported, even beside a pristine database.
    std::fs::write(
        root.path().join("application-settings-v2.json"),
        "invalid legacy file",
    )
    .unwrap();
    let mut store = store::Store::open(&path).unwrap();
    assert!(store.settings().unwrap().entries().is_empty());
    assert!(
        store::Store::open(&path).is_err(),
        "one process owns the database"
    );
    let (settings, _) = SettingsStore::default()
        .prepare_patch(
            "editor.desktop".into(),
            Some(Some("media".into())),
            Some(true),
        )
        .unwrap();
    let snapshot = compose(&catalog, &settings, &observations);
    store
        .replace(&snapshot, Some(&settings), &observations)
        .unwrap();
    assert_eq!(store.snapshot().unwrap().unwrap(), snapshot);
    drop(store);
    let mut reopened = store::Store::open(&path).unwrap();
    let restored = reopened.settings().unwrap();
    let preference = restored.for_application("editor.desktop").unwrap();
    assert_eq!(preference.category, "media");
    assert_eq!(preference.workspace_id.as_deref(), Some("4"));
    assert!(preference.new_workspace);
    assert_eq!(reopened.snapshot().unwrap().unwrap(), snapshot);
    let mut cached = reopened.observations().unwrap();
    cached.mark_unavailable("worker temporarily unavailable".into());
    let stale = compose(&catalog, &restored, &cached);
    assert!(!stale.inventory_available);
    assert_eq!(stale.entries.len(), snapshot.entries.len());
    assert!(
        stale
            .entries
            .iter()
            .flat_map(|r| &r.installations)
            .all(|i| i.stale)
    );
    assert!(
        stale
            .entries
            .iter()
            .flat_map(|r| &r.installations)
            .filter_map(|i| i.release.as_ref())
            .all(|r| r.stale)
    );
    let empty = compose(
        &Catalog::default(),
        &restored,
        &Observations {
            schema_version: 1,
            applications: Inventory {
                available: true,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    reopened.replace(&empty, None, &Default::default()).unwrap();
    assert!(reopened.snapshot().unwrap().unwrap().entries.is_empty());
    assert_eq!(
        reopened
            .settings()
            .unwrap()
            .for_application("editor.desktop"),
        Some(preference)
    );
    let invalid = root.path().join("catalog/not-a-catalog.db");
    std::fs::write(&invalid, b"not a database").unwrap();
    assert!(store::Store::open(&invalid).is_err());
    assert_eq!(std::fs::read(&invalid).unwrap(), b"not a database");
}

#[test]
fn missing_worker_keeps_desktop_apps_and_reports_unavailable_versions() {
    let (_root, catalog, _) = fixture();
    let snapshot = compose(
        &catalog,
        &SettingsStore::default(),
        &Observations::unavailable("worker offline".into()),
    );
    assert!(!snapshot.inventory_available);
    assert_eq!(snapshot.entries.len(), 2);
    assert!(snapshot.entries.iter().all(|r| r.installations.is_empty()));
    assert_eq!(snapshot.error.as_deref(), Some("worker offline"));
}

#[test]
fn failed_preference_write_rolls_back_catalog_and_remains_retryable() {
    let (root, catalog, observations) = fixture();
    let mut store = store::Store::open(&root.path().join("db/catalog.db")).unwrap();
    let initial = compose(&catalog, &SettingsStore::default(), &observations);
    store.replace(&initial, None, &observations).unwrap();
    let (settings, _) = SettingsStore::default()
        .prepare_patch("editor.desktop".into(), Some(Some("text".into())), None)
        .unwrap();
    let changed = compose(&catalog, &settings, &observations);
    store.test_sql("DROP TABLE preferences");
    assert!(
        store
            .replace(&changed, Some(&settings), &observations)
            .is_err()
    );
    assert_eq!(
        store.snapshot().unwrap().unwrap(),
        initial,
        "failed preference write must roll back earlier catalog writes"
    );
    store.test_sql("CREATE TABLE preferences (id TEXT PRIMARY KEY, payload TEXT NOT NULL) STRICT");
    store
        .replace(&changed, Some(&settings), &observations)
        .unwrap();
    assert_eq!(store.snapshot().unwrap().unwrap(), changed);
    assert_eq!(
        store
            .settings()
            .unwrap()
            .for_application("editor.desktop")
            .unwrap()
            .category,
        "text"
    );
}
