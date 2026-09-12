use super::*;

#[tokio::test]
async fn completed_operations_can_be_recovered_without_a_subscription() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop", "action":"launch"}),
    )
    .await?;
    let id = accepted["data"]["operation"]["id"]
        .as_str()
        .context("accepted id")?;
    let status = tokio::time::timeout(support::DEADLINE, async {
        loop {
            let status = call(
                &proxy,
                "applications.operation.status",
                json!({"operation_id":id}),
            )
            .await?;
            if status["data"]["operation_status"]["status"] == "completed" {
                return Result::<_>::Ok(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(status["data"]["operation_status"]["id"], id);
    let other = session.other_connection().await?;
    let stranger = zbus::Proxy::new(&other, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
    assert_eq!(
        call(
            &stranger,
            "applications.operation.status",
            json!({"operation_id":id})
        )
        .await?["error"]["code"],
        "request-not-found"
    );
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn metadata_only_refresh_replaces_launch_behavior_and_rejects_old_revision() -> Result<()> {
    let mut session = Session::start(false).await?;
    let proxy = session.proxy().await?;
    let original = call(&proxy, "applications.query", json!({})).await?;
    let revision = original["data"]["applications"]["revision"].clone();
    let directory = session.root.path().join("data");
    std::fs::write(
        session.root.path().join("data/applications/ok.desktop"),
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
        .at("/org/example/BusOnly", BusApplication(calls.clone()))
        .await?;
    app_connection.request_name("org.example.BusOnly").await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for action in ["launch", "desktop-action"] {
        let accepted = call(&proxy, "applications.execute", json!({"target_id":"org.example.BusOnly.desktop", "action":action, "desktop_action_id":"inspect"})).await?;
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
        .set(r#"[{"address":"0x123","class":"ok","pid":0,"title":"fixture-window"}]"#);
    session.compositor.disconnect();
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            let page = call(&proxy, "applications.query", json!({})).await?;
            if page["data"]["applications"]["applications"]
                .as_array()
                .context("application list")?
                .iter()
                .any(|app| app["id"] == "ok.desktop" && app["running"] == true)
            {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await??;
    drop(proxy);
    session.shutdown().await
}
