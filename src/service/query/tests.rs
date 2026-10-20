use std::fs;

use crate::{
    catalog::Catalog, hyprland::Snapshot, resources::ResourceSnapshot, settings::SettingsStore,
};

use super::{QueryParams, page};
use crate::service::identity::group_windows;

#[test]
fn target_revision_tracks_action_inputs_not_presentation() -> anyhow::Result<()> {
    use super::target_revision;
    use crate::{hyprland::Client, settings::ApplicationSettings};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.desktop");
    let desktop = "[Desktop Entry]\nType=Application\nName=Example\nExec=true\nActions=inspect;\n[Desktop Action inspect]\nName=Inspect\nExec=true --inspect\n";
    fs::write(&path, desktop)?;
    let load = || Catalog::from_paths(vec![directory.path().into()]);
    let catalog = load();
    let window: Client = serde_json::from_value(serde_json::json!({
        "address":"0x123", "class":"example", "pid":42, "workspace":{"id":1,"name":"1"}
    }))?;
    let settings = ApplicationSettings {
        category: "shell".into(),
        workspace_id: Some("1".into()),
    };
    let token =
        |catalog: &Catalog, windows: &[&Client], settings: &ApplicationSettings, available| {
            target_revision(
                "example.desktop",
                catalog.by_id("example.desktop"),
                windows,
                Some(settings),
                available,
            )
        };
    let original = token(&catalog, &[&window], &settings, true);
    assert_eq!(original as f64 as u64, original);
    let mut renamed = window.clone();
    renamed.title = "new song".into();
    renamed.focus_rank = 7;
    assert_eq!(original, token(&catalog, &[&renamed], &settings, true));
    assert_eq!(
        token(&catalog, &[&window, &renamed], &settings, true),
        token(&catalog, &[&renamed, &window], &settings, true)
    );
    for field in ["address", "pid", "class", "initial_class", "workspace"] {
        let mut changed = window.clone();
        match field {
            "address" => changed.address = "0x456".into(),
            "pid" => changed.pid += 1,
            "class" => changed.class = "other".into(),
            "initial_class" => changed.initial_class = "other".into(),
            _ => changed.workspace.id += 1,
        }
        assert_ne!(
            original,
            token(&catalog, &[&changed], &settings, true),
            "{field}"
        );
    }
    assert_ne!(original, token(&catalog, &[], &settings, true));
    assert_ne!(original, token(&catalog, &[&window], &settings, false));
    let changed_settings = ApplicationSettings {
        category: "media".into(),
        workspace_id: Some("4".into()),
    };
    assert_ne!(
        original,
        token(&catalog, &[&window], &changed_settings, true)
    );
    for (from, to) in [
        ("Exec=true\n", "Exec=changed\n"),
        ("Exec=true\n", "Exec=true\nTerminal=true\n"),
        ("Exec=true\n", "Exec=true\nPath=/different\n"),
        ("Exec=true\n", "Exec=true\nDBusActivatable=true\n"),
        ("Exec=true --inspect", "Exec=changed --inspect"),
    ] {
        fs::write(&path, desktop.replace(from, to))?;
        assert_ne!(
            original,
            token(&load(), &[&window], &settings, true),
            "{to}"
        );
    }
    fs::remove_file(path)?;
    assert_ne!(original, token(&load(), &[&window], &settings, true));
    Ok(())
}

#[test]
fn ranks_prefix_acronym_and_metadata_matches() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("google-contacts.desktop"),
        "[Desktop Entry]\nType=Application\nName=Google Contacts\nGenericName=Address Book\nKeywords=people;friends;\nCategories=Development;\nExec=true\n",
    )?;
    fs::write(
        directory.path().join("calculator.desktop"),
        "[Desktop Entry]\nType=Application\nName=Calculator\nComment=Perform arithmetic\nExec=true\n",
    )?;
    fs::write(
        directory.path().join("calculator-alt.desktop"),
        "[Desktop Entry]\nType=Application\nName=CALCULATOR\nExec=true\n",
    )?;
    fs::write(
        directory.path().join("manual.desktop"),
        "[Desktop Entry]\nType=Application\nName=Manual\nExec=true\nStartupWMClass=browser\nX-Shelllist-LaunchOnly=true\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let resources = ResourceSnapshot::default();
    let windows = Snapshot {
        revision: u64::MAX,
        available: true,
        clients: vec![serde_json::from_value(serde_json::json!({
            "address":"0x1", "class":"browser", "initialClass":"browser", "title":"Manual", "pid":42, "focusHistoryID":0
        }))?],
    };
    let search = |query: &str| {
        page(
            &catalog,
            &windows,
            &resources,
            &SettingsStore::load(None),
            &QueryParams {
                query: query.into(),
                category: String::new(),
                generation: 1,
                limit: 100,
            },
            group_windows(&catalog, &windows, &Default::default()),
            0,
        )
    };
    for (query, kind) in [
        ("  GOOGLE CONTACTS  ", "exact-name"),
        ("calculator.desktop", "exact-id"),
        ("google-contacts", "exact-id"),
        ("google-", "id-prefix"),
        ("ontact", "name-substring"),
        ("gle-con", "id-substring"),
        ("friends book", "terms"),
        ("friends gc", "terms"),
    ] {
        assert_eq!(search(query).applications[0].match_kind, kind, "{query}");
    }
    assert!(search("calculator missing").applications.is_empty());
    let all = search("");
    assert_eq!(
        all.applications
            .iter()
            .map(|app| app.identity.id.as_str())
            .collect::<Vec<_>>(),
        [
            "window-group:browser",
            "calculator-alt.desktop",
            "calculator.desktop",
            "google-contacts.desktop",
            "manual.desktop"
        ],
        "running groups rank first; case-insensitive name ties retain the ID tie-breaker"
    );
    assert_eq!(all.applications[0].runtime.running_count, 1);
    let shortcut = all
        .applications
        .iter()
        .find(|app| app.identity.id == "manual.desktop")
        .unwrap();
    assert_eq!(shortcut.identity.kind, "desktop-shortcut");
    assert!(
        !shortcut.runtime.running,
        "launch-only entries cannot claim matching windows"
    );
    let acronym = search("gc");
    assert!(acronym.revision < (1_u64 << 53));
    assert_eq!(acronym.revision as f64 as u64, acronym.revision);
    assert_eq!(acronym.applications.len(), 1);
    assert_eq!(acronym.applications[0].identity.name, "Google Contacts");
    assert_eq!(acronym.applications[0].match_kind, "acronym");
    assert_eq!(search("calc").applications[0].match_kind, "name-prefix");
    let metadata = search("people");
    assert_eq!(metadata.applications[0].match_kind, "metadata");
    assert!(metadata.applications[0].match_score > 0);
    let code = page(
        &catalog,
        &Snapshot::default(),
        &resources,
        &SettingsStore::load(None),
        &QueryParams {
            query: String::new(),
            category: "code".into(),
            generation: 2,
            limit: 100,
        },
        Default::default(),
        0,
    );
    assert_eq!(code.applications.len(), 1);
    assert_eq!(code.applications[0].identity.name, "Google Contacts");
    Ok(())
}
