use std::fs;

use crate::{
    catalog::Catalog,
    hyprland::{Client, Snapshot, Workspace},
    resources::ResourceSnapshot,
};

use super::{
    ApplicationAction, ApplicationService, ExecuteParams, QueryParams,
    application_window_addresses, combined_revision, page, resolve_target,
    resolve_target_with_cgroup, running_score,
};

#[test]
fn parses_application_actions() -> serde_json::Result<()> {
    assert_eq!(
        serde_json::from_str::<ApplicationAction>(r#""activate""#)?,
        ApplicationAction::Activate
    );
    assert_eq!(
        serde_json::from_str::<ApplicationAction>(r#""move-to-workspace""#)?,
        ApplicationAction::MoveToWorkspace
    );
    assert!(serde_json::from_str::<ApplicationAction>(r#""unknown""#).is_err());
    Ok(())
}

#[tokio::test]
async fn accepts_operations_before_reporting_their_result() -> anyhow::Result<()> {
    let service = ApplicationService::new();
    let mut events = service.subscribe_operations();
    let params = ExecuteParams {
        target_id: "missing-window-group".into(),
        action: ApplicationAction::Close,
        window_id: None,
        desktop_action_id: None,
        expected_revision: None,
        workspace_id: None,
    };
    let accepted = service.execute(params).await?;
    assert_eq!(accepted.status, "accepted");
    let running = events.recv().await?;
    let failed = events.recv().await?;
    assert_eq!(running.id, accepted.id);
    assert_eq!(running.status, "running");
    assert_eq!(failed.id, accepted.id);
    assert_eq!(failed.status, "failed");
    Ok(())
}

#[tokio::test]
async fn rejects_operations_for_stale_revisions() {
    let service = ApplicationService::new();
    let params = ExecuteParams {
        target_id: "missing-window-group".into(),
        action: ApplicationAction::Close,
        window_id: None,
        desktop_action_id: None,
        expected_revision: Some(u64::MAX),
        workspace_id: None,
    };
    assert!(service.execute(params).await.is_err());
}

#[test]
fn revisions_round_trip_exactly_through_javascript_numbers() {
    let catalog = Catalog::from_paths(Vec::new());
    let windows = Snapshot {
        revision: u64::MAX,
        ..Snapshot::default()
    };
    let revision = combined_revision(&catalog, &windows);
    assert!(revision <= (1_u64 << 53) - 1);
    assert_eq!(revision as f64 as u64, revision);
}

#[test]
fn focused_and_recent_windows_rank_first() {
    assert!(running_score(true, 0) > running_score(false, 1));
    assert!(running_score(false, 1) > running_score(false, 8));
    assert!(running_score(false, 8) > running_score(false, i64::MAX));
}

#[test]
fn ranks_prefix_acronym_and_metadata_matches() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("google-contacts.desktop"),
        "[Desktop Entry]\nType=Application\nName=Google Contacts\nGenericName=Address Book\nKeywords=people;friends;\nExec=true\n",
    )?;
    fs::write(
        directory.path().join("calculator.desktop"),
        "[Desktop Entry]\nType=Application\nName=Calculator\nComment=Perform arithmetic\nExec=true\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let resources = ResourceSnapshot::default();
    let search = |query: &str| {
        page(
            &catalog,
            &Snapshot::default(),
            &resources,
            &QueryParams {
                query: query.into(),
                generation: 1,
                limit: 100,
            },
        )
    };

    let acronym = search("gc");
    assert_eq!(acronym.applications.len(), 1);
    assert_eq!(acronym.applications[0].identity.name, "Google Contacts");
    assert_eq!(acronym.applications[0].match_kind, "acronym");
    let prefix = search("calc");
    assert_eq!(prefix.applications[0].match_kind, "name-prefix");
    let metadata = search("people");
    assert_eq!(metadata.applications[0].match_kind, "metadata");
    assert!(metadata.applications[0].match_score > 0);
    Ok(())
}

#[test]
fn selects_all_application_windows_for_closing() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("example.desktop"),
        "[Desktop Entry]\nType=Application\nName=Example\nExec=true\nStartupWMClass=example\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let client = |address: &str, class: &str| Client {
        address: address.into(),
        class: class.into(),
        initial_class: class.into(),
        title: class.into(),
        pid: 42,
        workspace: Workspace::default(),
        focus_rank: 0,
        mapped: true,
    };
    let windows = crate::hyprland::Snapshot {
        available: true,
        revision: 1,
        clients: vec![
            client("0x1", "example"),
            client("0x2", "example"),
            client("0x3", "other"),
        ],
    };

    assert_eq!(
        application_window_addresses(&catalog, &windows, "example.desktop"),
        ["0x1", "0x2"]
    );
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
    assert_eq!(
        resolve_target_with_cgroup(
            &catalog,
            &window,
            Some("/app.slice/app-Hyprland-btop-a1b2c3d4.scope"),
        ),
        "btop.desktop"
    );
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
    let result = page(
        &catalog,
        &Snapshot::default(),
        &ResourceSnapshot::default(),
        &QueryParams {
            query: String::new(),
            generation: 1,
            limit: 10,
        },
    );
    assert_eq!(result.applications[0].identity.kind, "desktop-shortcut");
    Ok(())
}

#[test]
fn resolves_unique_reverse_dns_class_suffix() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("yazi.desktop"),
        "[Desktop Entry]\nType=Application\nName=Yazi\nExec=true\n",
    )?;
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let window = Client {
        address: "0x1".into(),
        class: "com.laufan.yazi".into(),
        initial_class: "com.laufan.yazi".into(),
        title: "Yazi".into(),
        pid: 42,
        workspace: Workspace::default(),
        focus_rank: 0,
        mapped: true,
    };
    assert_eq!(resolve_target(&catalog, &window), "yazi.desktop");
    Ok(())
}
