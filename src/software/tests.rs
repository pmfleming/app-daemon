use super::{Catalog, Observations, Release, SettingsStore, compose, read_worker, store};
use anyhow::{Context, Result};
use serde_json::json;

#[test]
fn update_ownership_and_release_states_are_domain_facts() -> Result<()> {
    use super::{ReleaseState as State, UpdateRoute as Route};
    let (_root, catalog, mut observations) = fixture()?;
    let snapshot = compose(&catalog, &SettingsStore::default(), &observations);
    let vendor = &snapshot.entries[0].installations[0];
    assert_eq!(vendor.update_route, Some(Route::Vendor));
    assert_eq!(vendor.release_state, Some(State::Newer));
    observations.applications.packages[0].vendor_tool = None;
    observations.applications.packages[0].kind = Some("local-helper".into());
    let snapshot = compose(&catalog, &SettingsStore::default(), &observations);
    assert_eq!(
        snapshot.entries[0].installations[0].update_route,
        Some(Route::Local)
    );
    assert_eq!(
        snapshot.entries[0].installations[0].release_state,
        Some(State::Local)
    );
    let mut release = Release {
        version: Some("2.0".into()),
        source: "official".into(),
        checked_at: Some(10),
        attempted_at: Some(10),
        stale: false,
        error: None,
    };
    assert_eq!(
        super::release_state(Some(&release), Some("2.0"), Route::Nixos),
        State::Current
    );
    assert_eq!(
        super::release_state(Some(&release), Some("3.0"), Route::Nixos),
        State::Different
    );
    assert_eq!(
        super::release_state(Some(&release), Some("1.9b"), Route::Nixos),
        State::Different
    );
    release.stale = true;
    assert_eq!(
        super::release_state(Some(&release), Some("1.0"), Route::Nixos),
        State::Cached
    );
    release.error = Some("reader unavailable".into());
    assert_eq!(
        super::release_state(Some(&release), Some("1.0"), Route::Nixos),
        State::Failed
    );
    Ok(())
}

fn fixture() -> Result<(tempfile::TempDir, Catalog, Observations)> {
    let root = tempfile::tempdir()?;
    for (id, name, category) in [
        ("editor", "Editor", "Development"),
        ("untracked", "Untracked", ""),
    ] {
        std::fs::write(
            root.path().join(format!("{id}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={id}\nCategories={category};\n"
            ),
        )?;
    }
    let catalog = Catalog::from_paths(vec![root.path().to_owned()]);
    let observations = Observations::decode(&serde_json::to_vec(&json!({"schema_version":1,
        "applications":{"available":true,"total":3,"packages":[
            {"scope":"system","name":"editor-package","version":"1.0","desktop_ids":["editor.desktop"],"vendor_tool":"codex"},
            {"scope":"user:test","name":"editor-package","version":"0.9","desktop_ids":["editor.desktop"]},
            {"scope":"system","name":"Untracked","version":"3.0","kind":"command-line"}
        ]}, "tools":[{"tool":"codex","installed_version":"2.0","latest_version":"2.1",
            "latest_release":{"source":"https://example.test/releases/2.1"},"last_successful_check":100,"last_attempt":101,"stale":false}]
    }))?)?;
    Ok((root, catalog, observations))
}

#[tokio::test]
async fn worker_ingestion_enforces_output_bounds_decode_validation_and_exit_status() -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};
    let (root, _, observations) = fixture()?;
    let worker = root.path().join("worker");
    fs::write(&worker, "#!/bin/sh\ncat \"$0.json\"\n")?;
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;
    let output = worker.with_extension("json");
    let mut bytes = serde_json::to_vec(&observations)?;
    for length in [bytes.len(), 2_097_152, 2_097_153] {
        bytes.resize(length, b' ');
        fs::write(&output, &bytes)?;
        let result = read_worker(&worker).await;
        if length <= 2_097_152 {
            assert_eq!(result?.applications.packages.len(), 3);
        } else {
            assert!(
                result
                    .err()
                    .context("oversized worker accepted")?
                    .to_string()
                    .contains("exceeds 2 MiB")
            );
        }
    }
    for invalid in [
        "{",
        r#"{"schema_version":0,"applications":{"available":true,"total":0,"packages":[]},"tools":[]}"#,
    ] {
        fs::write(&output, invalid)?;
        assert!(read_worker(&worker).await.is_err());
    }
    fs::write(&worker, "#!/bin/sh\nexit 7\n")?;
    assert!(
        read_worker(&worker)
            .await
            .err()
            .context("nonzero worker accepted")?
            .to_string()
            .contains("application-status unavailable")
    );
    assert!(
        read_worker(std::path::Path::new("relative-worker"))
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn identity_scopes_shared_packages_and_missing_worker_remain_distinct() -> Result<()> {
    let (_root, catalog, mut observations) = fixture()?;
    let settings = SettingsStore::default();
    let snapshot = compose(&catalog, &settings, &observations);
    let editor = &snapshot.entries[0];
    assert_eq!(
        (&*editor.id, &*editor.name, &*editor.category),
        ("editor.desktop", "Editor", "code")
    );
    assert_eq!(editor.installations.len(), 2);
    let versions = editor
        .installations
        .iter()
        .map(|i| {
            (
                i.declared_version.as_deref(),
                i.active_version.as_deref(),
                i.release.as_ref().and_then(|r| r.version.as_deref()),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        versions,
        [
            (Some("1.0"), Some("2.0"), Some("2.1")),
            (Some("0.9"), None, None)
        ]
    );
    assert!(
        snapshot.entries[1].installations.is_empty(),
        "display names never establish identity"
    );
    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.entries[2].kind, "command-line");
    for _ in 0..2 {
        observations.applications.packages.push(serde_json::from_value(json!({
            "scope":"zz-shared", "name":"shared", "desktop_ids":["editor.desktop","untracked.desktop"]
        }))?);
    }
    let shared = compose(&catalog, &settings, &observations);
    assert_eq!(
        shared.entries[0].installations.len(),
        3,
        "duplicate observations collapse"
    );
    assert_eq!(
        shared.entries[0].installations[2],
        shared.entries[1].installations[0]
    );
    observations.applications.packages.reverse();
    let reversed = compose(&catalog, &settings, &observations);
    assert_eq!(
        (shared.entries, shared.revision),
        (reversed.entries, reversed.revision)
    );
    let missing = compose(
        &catalog,
        &settings,
        &Observations::unavailable("worker offline".into()),
    );
    assert!(!missing.inventory_available);
    assert_eq!(missing.entries.len(), 2);
    assert!(missing.entries.iter().all(|r| r.installations.is_empty()));
    assert_eq!(missing.error.as_deref(), Some("worker offline"));
    Ok(())
}

#[test]
fn release_precedence_fallback_and_staleness_preserve_observation_provenance() -> Result<()> {
    let (_root, catalog, mut observations) = fixture()?;
    let fallback = Release {
        version: Some("1.1".into()),
        source: "package-source".into(),
        checked_at: Some(90),
        attempted_at: Some(91),
        stale: false,
        error: Some("retained diagnostic".into()),
    };
    observations.applications.packages[0].release = Some(fallback);
    for (tool, (version, attempted, active, source, stale)) in [
        (
            json!({"tool":"other"}),
            ("1.1", 91, None, "package-source", false),
        ),
        (
            json!({"tool":"codex", "installed_version":"2.0", "latest_version":"2.1"}),
            ("1.1", 91, Some("2.0"), "package-source", false),
        ),
        (
            json!({"tool":"codex", "latest_version":"2.1", "latest_release":{"source":"tool-source"}, "last_successful_check":100, "error":"build failed"}),
            ("2.1", 100, None, "tool-source", true),
        ),
        (
            json!({"tool":"codex", "latest_version":"2.1", "latest_release":{"source":"tool-source"}, "last_successful_check":100, "last_attempt":101, "stale":false}),
            ("2.1", 101, None, "tool-source", false),
        ),
    ] {
        observations.tools = vec![tool];
        for unavailable in [false, true] {
            observations.stale = unavailable;
            let snapshot = compose(&catalog, &SettingsStore::default(), &observations);
            let installation = &snapshot.entries[0].installations[0];
            assert_eq!(installation.active_version.as_deref(), active);
            assert_eq!(
                installation.version_origin,
                if active.is_some() {
                    "active-profile"
                } else {
                    "generation"
                }
            );
            assert_eq!(installation.stale, unavailable);
            assert_eq!(
                installation.release,
                Some(Release {
                    version: Some(version.into()),
                    source: source.into(),
                    checked_at: Some(if source == "tool-source" { 100 } else { 90 }),
                    attempted_at: Some(attempted),
                    stale: stale || unavailable,
                    error: (source == "package-source").then(|| "retained diagnostic".into()),
                })
            );
        }
    }
    Ok(())
}

#[test]
fn turso_roundtrip_retry_removals_and_pristine_identity() -> Result<()> {
    let (root, catalog, observations) = fixture()?;
    let path = root.path().join("catalog/catalog.db");
    let legacy = root.path().join("application-settings-v2.json");
    std::fs::write(&legacy, "invalid legacy file")?;
    let mut store = store::Store::open(&path)?;
    assert!(store.settings()?.entries().is_empty());
    assert!(
        store::Store::open(&path).is_err(),
        "one process owns the database"
    );
    let (settings, _) = SettingsStore::default().prepare_patch(
        "editor.desktop".into(),
        Some(Some("media".into())),
        Some(true),
    )?;
    let snapshot = compose(&catalog, &settings, &observations);
    store.replace(&snapshot, Some(&settings), &observations)?;
    assert_eq!(store.snapshot()?.as_ref(), Some(&snapshot));
    drop(store);
    let mut reopened = store::Store::open(&path)?;
    let restored = reopened.settings()?;
    let preference = restored
        .for_application("editor.desktop")
        .ok_or_else(|| anyhow::anyhow!("missing preference"))?;
    assert_eq!(
        (
            &*preference.category,
            preference.workspace_id.as_deref(),
            preference.new_workspace
        ),
        ("media", Some("4"), true)
    );
    assert_eq!(reopened.snapshot()?.as_ref(), Some(&snapshot));
    let mut cached = reopened.observations()?;
    cached.mark_unavailable("worker temporarily unavailable".into());
    let stale = compose(&catalog, &restored, &cached);
    assert!(!stale.inventory_available);
    assert_eq!(stale.entries.len(), 3);
    assert!(
        stale
            .entries
            .iter()
            .flat_map(|r| &r.installations)
            .all(|i| { i.stale && i.release.as_ref().is_none_or(|r| r.stale) })
    );
    let (changed, _) =
        restored.prepare_patch("editor.desktop".into(), Some(Some("text".into())), None)?;
    let updated = compose(&catalog, &changed, &observations);
    reopened.test_sql("DROP TABLE preferences");
    assert!(
        reopened
            .replace(&updated, Some(&changed), &observations)
            .is_err()
    );
    assert_eq!(
        reopened.snapshot()?.as_ref(),
        Some(&snapshot),
        "failed preference writes roll back earlier catalog writes"
    );
    reopened
        .test_sql("CREATE TABLE preferences (id TEXT PRIMARY KEY, payload TEXT NOT NULL) STRICT");
    reopened.replace(&updated, Some(&changed), &observations)?;
    assert_eq!(reopened.snapshot()?, Some(updated));
    assert_eq!(
        reopened.settings()?.for_application("editor.desktop"),
        changed.for_application("editor.desktop")
    );
    let empty = compose(&Catalog::default(), &restored, &Observations::default());
    reopened.replace(&empty, None, &Default::default())?;
    assert!(reopened.snapshot()?.is_some_and(|s| s.entries.is_empty()));
    assert_eq!(
        reopened.settings()?.for_application("editor.desktop"),
        changed.for_application("editor.desktop")
    );
    assert_eq!(std::fs::read(&legacy)?, b"invalid legacy file");
    let invalid = root.path().join("catalog/not-a-catalog.db");
    std::fs::write(&invalid, b"not a database")?;
    assert!(store::Store::open(&invalid).is_err());
    assert_eq!(std::fs::read(&invalid)?, b"not a database");
    Ok(())
}
