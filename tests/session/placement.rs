//! Private bus/compositor regressions. No host application or workspace changes.
use super::support::{Session, call, events, operation};
use anyhow::{Context, Result};
use serde_json::{Value, json};

struct Manager;
#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    fn get_unit(&self, name: &str) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        if !name.starts_with("app-") || !name.ends_with(".service") {
            return Err(zbus::fdo::Error::Failed("not a launch service".into()));
        }
        Ok("/org/freedesktop/systemd1/unit/fixture".try_into().unwrap())
    }
}
struct Service;
#[zbus::interface(name = "org.freedesktop.systemd1.Service")]
impl Service {
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> u32 {
        std::process::id()
    }
}
async fn manager(session: &Session) -> Result<zbus::Connection> {
    let connection = session.other_connection().await?;
    connection
        .object_server()
        .at("/org/freedesktop/systemd1", Manager)
        .await?;
    connection
        .object_server()
        .at("/org/freedesktop/systemd1/unit/fixture", Service)
        .await?;
    connection.request_name("org.freedesktop.systemd1").await?;
    Ok(connection)
}
fn window(address: &str, class: &str, pid: u32, workspace: i64) -> Value {
    json!({"address":address, "class":class, "pid":pid, "workspace":{"id":workspace,"name":workspace.to_string()}})
}
async fn handoff(stream: &mut zbus::proxy::SignalStream<'_>, id: &str) -> Result<Value> {
    loop {
        let result = operation(stream, id, "running").await?;
        if result.get("launch_backend").is_some() {
            return Ok(result);
        }
    }
}

#[tokio::test]
async fn category_overrides_context_for_all_launch_paths_despite_scope_migration() -> Result<()> {
    let mut session = Session::start(true).await?;
    let _manager = manager(&session).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    // The manager's MainPID lives in the test runner's unrelated cgroup, exactly
    // the ownership situation after Spotify/Chromium migrates out of UWSM's unit.
    for (category, workspace) in [
        ("shell", 1),
        ("browser", 2),
        ("code", 3),
        ("media", 4),
        ("text", 5),
    ] {
        assert_eq!(
            call(
                &proxy,
                "applications.settings.update",
                json!({"target_id":"ok.desktop","category":category})
            )
            .await?["ok"],
            true
        );
        for action in ["launch", "activate", "desktop-action"] {
            session.compositor.set("[]");
            let accepted = call(&proxy, "applications.execute", json!({"target_id":"ok.desktop","action":action,"desktop_action_id":"inspect","workspace_id":"9"})).await?;
            let id = accepted["data"]["operation"]["id"]
                .as_str()
                .context("operation id")?;
            let progress = handoff(&mut stream, id).await?;
            assert_eq!(progress["placement"]["status"], "pending");
            assert_eq!(progress["placement"]["workspace_id"], workspace.to_string());
            session
                .compositor
                .set(&json!([window("0x123", "ok", std::process::id(), 9)]).to_string());
            let result = operation(&mut stream, id, "completed").await?;
            assert_eq!(result["placement"]["status"], "placed", "{result}");
            assert_eq!(result["launch_backend"], "uwsm-app");
            let recovered = call(
                &proxy,
                "applications.operation.status",
                json!({"operation_id":id}),
            )
            .await?;
            assert_eq!(
                recovered["data"]["operation_status"]["placement"],
                result["placement"]
            );
        }
    }
    let moves = session
        .compositor
        .commands()
        .into_iter()
        .filter(|command| command.contains("hl.dsp.window.move"))
        .collect::<Vec<_>>();
    assert_eq!(moves.len(), 15);
    assert!(
        moves
            .iter()
            .all(|command| !command.contains("workspace = '9'"))
    );
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn singleton_new_window_is_placed_but_existing_window_and_activation_stay_put() -> Result<()>
{
    let mut session = Session::start(true).await?;
    // No systemd service fixture: only the established singleton process proves
    // ownership. Its old window is never an eligible placement candidate.
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    let old = window("0x100", "ok", std::process::id(), 9);
    session.compositor.set(&json!([old]).to_string());
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop","action":"launch","workspace_id":"3"}),
    )
    .await?;
    let id = accepted["data"]["operation"]["id"].as_str().unwrap();
    handoff(&mut stream, id).await?;
    session
        .compositor
        .set(&json!([old, window("0x200", "ok", std::process::id(), 9)]).to_string());
    assert_eq!(
        operation(&mut stream, id, "completed").await?["placement"]["status"],
        "placed"
    );
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop","action":"activate","workspace_id":"3"}),
    )
    .await?;
    let result = operation(
        &mut stream,
        accepted["data"]["operation"]["id"].as_str().unwrap(),
        "completed",
    )
    .await?;
    assert!(result.get("placement").is_none());
    let commands = session.compositor.commands();
    let moves = commands
        .iter()
        .filter(|command| command.contains("hl.dsp.window.move"))
        .collect::<Vec<_>>();
    assert_eq!(moves.len(), 1);
    assert!(moves[0].contains("address:0x200"));
    drop(proxy);
    session.shutdown().await
}

struct BusApplication;
#[zbus::interface(name = "org.freedesktop.Application")]
impl BusApplication {
    fn activate(&self, _platform: std::collections::HashMap<String, zbus::zvariant::OwnedValue>) {}
    fn activate_action(
        &self,
        _action: &str,
        _parameters: Vec<zbus::zvariant::OwnedValue>,
        _platform: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
    }
}
#[tokio::test]
async fn dbus_owner_proves_new_window_ownership_for_launch_and_desktop_action() -> Result<()> {
    let mut session = Session::start(true).await?;
    let app = session.other_connection().await?;
    app.object_server()
        .at("/org/example/Bus_Only", BusApplication)
        .await?;
    app.request_name("org.example.Bus-Only").await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    call(
        &proxy,
        "applications.settings.update",
        json!({"target_id":"org.example.Bus-Only.desktop","category":"text"}),
    )
    .await?;
    for action in ["activate", "launch", "desktop-action"] {
        session.compositor.set("[]");
        let accepted = call(&proxy,"applications.execute",json!({"target_id":"org.example.Bus-Only.desktop","action":action,"desktop_action_id":"inspect","workspace_id":"1"})).await?;
        let id = accepted["data"]["operation"]["id"].as_str().unwrap();
        assert_eq!(
            handoff(&mut stream, id).await?["launch_backend"],
            "dbus-activation"
        );
        session.compositor.set(
            &json!([window(
                "0x300",
                "org.example.Bus-Only",
                std::process::id(),
                1
            )])
            .to_string(),
        );
        let result = operation(&mut stream, id, "completed").await?;
        assert_eq!(result["placement"]["status"], "placed", "{result}");
        assert_eq!(result["placement"]["workspace_id"], "5");
    }
    assert!(!session.state_path("arguments").exists());
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn rejected_and_unconfirmed_moves_preserve_launch_receipt_and_report_partial_failure()
-> Result<()> {
    let mut session = Session::start(true).await?;
    let _manager = manager(&session).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for accept in [false, true] {
        session.compositor.set("[]");
        session.compositor.moves(accept, false);
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({"target_id":"ok.desktop","action":"launch","workspace_id":"3"}),
        )
        .await?;
        let id = accepted["data"]["operation"]["id"].as_str().unwrap();
        handoff(&mut stream, id).await?;
        session
            .compositor
            .set(&json!([window("0x123", "ok", std::process::id(), 9)]).to_string());
        let result = operation(&mut stream, id, "completed").await?;
        assert_eq!(result["placement"]["status"], "failed", "{result}");
        assert_eq!(result["launch_backend"], "uwsm-app");
        assert!(
            result["placement"]["reason"]
                .as_str()
                .unwrap()
                .contains(if accept { "not confirmed" } else { "rejected" })
        );
    }
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn unrelated_same_class_and_ambiguous_windows_are_never_moved() -> Result<()> {
    let mut session = Session::start(true).await?;
    let _manager = manager(&session).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for windows in [
        json!([window("0x123", "ok", 1, 9)]),
        json!([
            window("0x123", "ok", std::process::id(), 9),
            window("0x456", "ok", std::process::id(), 9)
        ]),
    ] {
        session.compositor.set("[]");
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({"target_id":"ok.desktop","action":"launch","workspace_id":"3"}),
        )
        .await?;
        let id = accepted["data"]["operation"]["id"].as_str().unwrap();
        handoff(&mut stream, id).await?;
        session.compositor.set(&windows.to_string());
        let result = operation(&mut stream, id, "completed").await?;
        assert_eq!(result["placement"]["status"], "unavailable", "{result}");
    }
    assert!(
        !session
            .compositor
            .commands()
            .iter()
            .any(|command| command.starts_with("dispatch"))
    );
    drop(proxy);
    session.shutdown().await
}
