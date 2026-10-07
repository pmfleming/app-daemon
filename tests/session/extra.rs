use super::support::{self, Session, call, events, operation};
use anyhow::{Context, Result};
use serde_json::json;

#[tokio::test]
async fn metadata_only_refresh_replaces_launch_behavior_and_rejects_old_revision() -> Result<()> {
    let mut session = Session::start(false).await?;
    let proxy = session.proxy().await?;
    let original = call(&proxy, "applications.query", json!({})).await?;
    let revision = original["data"]["applications"]["revision"].clone();
    let directory = session.root.path().join("data");
    let entry_path = directory.join("applications/ok.desktop");
    let original_entry = std::fs::read_to_string(&entry_path)?;
    // Every launch-only metadata change must invalidate an otherwise identical
    // row; changing several fields at once could hide a missing revision input.
    for (from, to) in [
        ("Exec=true\n", "Exec=changed-command\n"),
        ("Exec=true\n", "Exec=true\nTerminal=true\n"),
        ("Exec=true\n", "Exec=true\nPath=/changed\n"),
        ("Exec=true\n", "Exec=true\nDBusActivatable=true\n"),
        ("Exec=true --inspect", "Exec=changed-action --inspect"),
    ] {
        std::fs::write(&entry_path, original_entry.replace(from, to))?;
        let changed = call(&proxy, "applications.refresh", json!({})).await?;
        assert_ne!(
            revision, changed["data"]["applications"]["revision"],
            "{to}"
        );
    }
    std::fs::write(
        &entry_path,
        format!(
            "[Desktop Entry]\nType=Application\nName=ok\nExec=updated-command --flag\nTerminal=true\nPath={}\nActions=inspect;\n[Desktop Action inspect]\nName=Inspect\nExec=updated-action --inspect\n",
            directory.display()
        ),
    )?;
    let refreshed = call(&proxy, "applications.refresh", json!({})).await?;
    assert_ne!(revision, refreshed["data"]["applications"]["revision"]);
    let rejected = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop", "action":"launch", "expected_revision":revision}),
    )
    .await?;
    assert_eq!(rejected["ok"], false);
    let mut stream = events(&proxy).await?;
    for (action, expected) in [
        ("launch", "--\nupdated-command\n--flag\n"),
        ("desktop-action", "--\nupdated-action\n--inspect\n"),
    ] {
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({"target_id":"ok.desktop", "action":action, "desktop_action_id":"inspect"}),
        )
        .await?;
        operation(
            &mut stream,
            accepted["data"]["operation"]["id"]
                .as_str()
                .context("operation id")?,
            "completed",
        )
        .await?;
        assert_eq!(
            std::fs::read_to_string(session.state_path("terminal-arguments"))?,
            expected
        );
        assert_eq!(
            std::fs::read_to_string(session.state_path("working-directory"))?.trim(),
            directory.to_str().unwrap()
        );
        let arguments = std::fs::read_to_string(session.state_path("arguments"))?;
        for required in [
            "--service-type=exec",
            "--property=ExitType=cgroup",
            "--same-dir",
        ] {
            assert!(arguments.lines().any(|arg| arg == required), "{arguments}");
        }
    }
    drop(proxy);
    session.shutdown().await
}

struct BusApplication(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

#[zbus::interface(name = "org.freedesktop.Application")]
impl BusApplication {
    fn activate(
        &self,
        _platform_data: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
        self.0.lock().unwrap().push("activate".into());
    }
    fn activate_action(
        &self,
        action: &str,
        _parameters: Vec<zbus::zvariant::OwnedValue>,
        _platform_data: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
        self.0.lock().unwrap().push(action.into());
    }
}

#[tokio::test]
async fn dbus_only_desktop_entries_activate_and_invoke_actions_without_exec() -> Result<()> {
    let mut session = Session::start(true).await?;
    let app_connection = session.other_connection().await?;
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app_connection
        .object_server()
        .at("/org/example/Bus_Only", BusApplication(calls.clone()))
        .await?;
    app_connection.request_name("org.example.Bus-Only").await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for action in ["launch", "desktop-action"] {
        let accepted = call(&proxy, "applications.execute", json!({"target_id":"org.example.Bus-Only.desktop", "action":action, "desktop_action_id":"inspect"})).await?;
        let result = operation(
            &mut stream,
            accepted["data"]["operation"]["id"]
                .as_str()
                .context("operation id")?,
            "completed",
        )
        .await?;
        assert_eq!(result["launch_backend"], "dbus-activation");
    }
    assert_eq!(&*calls.lock().unwrap(), &["activate", "inspect"]);
    assert!(
        !session.state_path("arguments").exists(),
        "D-Bus activation must not invoke a command launcher"
    );
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn compositor_failures_and_reconnects_reconcile_without_host_desktop_access() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    session.compositor.set("invalid json");
    let unavailable = call(&proxy, "applications.refresh", json!({})).await?;
    assert_eq!(
        unavailable["data"]["applications"]["hyprland_available"],
        false
    );
    session
        .compositor
        .set(r#"[{"address":"0x123","class":"ok","pid":0,"title":"fixture-window"},{"address":"0x0","class":"ok"},{"address":"0x456","class":"ok","mapped":false}]"#);
    session.compositor.disconnect();
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            let page = call(&proxy, "applications.query", json!({})).await?;
            if let Some(app) = page["data"]["applications"]["applications"]
                .as_array()
                .context("application list")?
                .iter()
                .find(|app| app["id"] == "ok.desktop" && app["running"] == true)
            {
                assert_eq!(
                    app["instances"].as_array().context("window list")?.len(),
                    1,
                    "invalid and unmapped windows must not appear"
                );
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await??;
    drop(proxy);
    session.shutdown().await
}
