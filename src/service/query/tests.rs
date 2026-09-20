use std::fs;

use crate::{
    catalog::Catalog,
    hyprland::{Client, Snapshot, Workspace},
    resources::ResourceSnapshot,
    settings::SettingsStore,
};

use super::{QueryParams, page};
use crate::service::identity::{group_windows, resolve_target, resolve_target_with_cgroup};

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
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let resources = ResourceSnapshot::default();
    let windows = Snapshot {
        revision: u64::MAX,
        ..Default::default()
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
            Default::default(),
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
    ] {
        assert_eq!(search(query).applications[0].match_kind, kind, "{query}");
    }
    let acronym = search("gc");
    assert!(acronym.revision < (1_u64 << 53));
    assert_eq!(acronym.revision as f64 as u64, acronym.revision);
    assert_eq!(acronym.applications.len(), 1);
    assert_eq!(acronym.applications[0].identity.name, "Google Contacts");
    assert_eq!(acronym.applications[0].match_kind, "acronym");
    let prefix = search("calc");
    assert_eq!(prefix.applications[0].match_kind, "name-prefix");
    let metadata = search("people");
    assert_eq!(metadata.applications[0].match_kind, "metadata");
    assert!(metadata.applications[0].match_score > 0);

    let categories = SettingsStore::load(None);
    let code = page(
        &catalog,
        &Snapshot::default(),
        &resources,
        &categories,
        &QueryParams {
            query: String::new(),
            category: "code".into(),
            generation: 2,
            limit: 100,
        },
        Default::default(),
    );
    assert_eq!(code.applications.len(), 1);
    assert_eq!(code.applications[0].identity.name, "Google Contacts");
    Ok(())
}

#[test]
fn resolves_uwsm_cgroup_before_terminal_window_class() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("btop.desktop"),
        "[Desktop Entry]\nType=Application\nName=btop\nExec=btop\nTerminal=true\n",
    )?;
    fs::write(
        directory.path().join("com.mitchellh.ghostty.desktop"),
        "[Desktop Entry]\nType=Application\nName=Ghostty\nExec=ghostty\n",
    )?;
    fs::write(
        directory.path().join("android-studio.desktop"),
        "[Desktop Entry]\nType=Application\nName=Android Studio\nExec=android-studio\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let window = Client {
        address: "0x1".into(),
        class: "com.mitchellh.ghostty".into(),
        initial_class: "com.mitchellh.ghostty".into(),
        title: "btop".into(),
        pid: 42,
        workspace: Workspace::default(),
        focus_rank: 0,
        mapped: true,
    };
    for path in [
        "/app.slice/app-Hyprland-btop-a1b2c3d4.scope",
        "/app-btop@12345678.service/child",
        "/app-flatpak-btop-433952237.scope",
        "/app-dbus-btop.service",
        "/app-btop.service",
        r"/app-\x62top@deadbeef.service",
    ] {
        assert_eq!(
            resolve_target_with_cgroup(&catalog, &window, Some(path)),
            "btop.desktop",
            "{path}"
        );
    }
    for path in [
        "/app-daemon.service",
        "/user@1000.service",
        "/btop.service",
        "/app-unknown.service",
        "/app-btop.scope",
        "/app-notbtop@123.service",
        "/app-btop@not-hex.service",
        "/app-btop@.service",
    ] {
        assert_eq!(
            resolve_target_with_cgroup(&catalog, &window, Some(path)),
            "com.mitchellh.ghostty.desktop",
            "{path}"
        );
    }
    assert_eq!(
        resolve_target_with_cgroup(
            &catalog,
            &window,
            Some("/app.slice/app-Hyprland-com.mitchellh.ghostty@a1b2c3d4.service"),
        ),
        "com.mitchellh.ghostty.desktop"
    );
    assert_eq!(
        resolve_target_with_cgroup(
            &catalog,
            &window,
            Some(r"/app.slice/app-Hyprland-android\x2dstudio-deadbeef.scope"),
        ),
        "android-studio.desktop"
    );
    Ok(())
}

#[test]
fn launch_only_entries_remain_shortcuts_without_claiming_windows() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("manual.desktop"),
        "[Desktop Entry]\nType=Application\nName=Manual\nExec=xdg-open https://example.test\nStartupWMClass=browser\nX-Shelllist-LaunchOnly=true\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let window = Client {
        address: "0x1".into(),
        class: "browser".into(),
        initial_class: "browser".into(),
        title: "Manual".into(),
        pid: 42,
        workspace: Workspace::default(),
        focus_rank: 0,
        mapped: true,
    };
    assert_eq!(resolve_target(&catalog, &window), "window-group:browser");
    let windows = Snapshot {
        available: true,
        clients: vec![window],
        ..Default::default()
    };
    let result = page(
        &catalog,
        &windows,
        &ResourceSnapshot::default(),
        &SettingsStore::load(None),
        &QueryParams {
            query: String::new(),
            category: String::new(),
            generation: 1,
            limit: 10,
        },
        group_windows(&catalog, &windows),
    );
    let shortcut = result
        .applications
        .iter()
        .find(|app| app.identity.id == "manual.desktop")
        .unwrap();
    assert_eq!(shortcut.identity.kind, "desktop-shortcut");
    assert!(!shortcut.runtime.running);
    assert_eq!(result.applications[0].identity.id, "window-group:browser");
    assert_eq!(result.applications[0].runtime.running_count, 1);
    Ok(())
}
