//! Real private D-Bus and compositor IPC; no running desktop is modified.
use super::support::{DEADLINE, Session, call, events, operation, tool};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use zbus::zvariant::OwnedObjectPath;

const PATH: &str = "/org/freedesktop/systemd1/unit/owned";
type Unit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);
struct Manager;
#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    fn get_unit(&self, _name: &str) -> OwnedObjectPath {
        PATH.try_into().unwrap()
    }
    fn list_units(&self) -> Vec<Unit> {
        vec![(
            "app-ok@123.service".into(),
            "fixture".into(),
            "loaded".into(),
            "active".into(),
            "running".into(),
            String::new(),
            PATH.try_into().unwrap(),
            0,
            String::new(),
            "/".try_into().unwrap(),
        )]
    }
}
struct Service {
    pid: u32,
    mode: Arc<AtomicUsize>,
}
#[zbus::interface(name = "org.freedesktop.systemd1.Service")]
impl Service {
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> u32 {
        if self.mode.load(Ordering::SeqCst) == 1 {
            0
        } else {
            self.pid
        }
    }
    #[zbus(property, name = "ExecMainPID")]
    fn exec_main_pid(&self) -> u32 {
        self.pid
    }
}
struct UnitState {
    mode: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}
#[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
impl UnitState {
    #[zbus(property)]
    fn active_state(&self) -> &str {
        if self.mode.load(Ordering::SeqCst) == 2 {
            "inactive"
        } else {
            "active"
        }
    }
    #[zbus(property, name = "InvocationID")]
    fn invocation_id(&self) -> Vec<u8> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        vec![
            if self.mode.load(Ordering::SeqCst) == 3 {
                (read % 250 + 1) as u8
            } else {
                1
            };
            16
        ]
    }
}
async fn manager(
    session: &Session,
    pid: u32,
    mode: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
) -> Result<zbus::Connection> {
    let connection = session.other_connection().await?;
    connection
        .object_server()
        .at("/org/freedesktop/systemd1", Manager)
        .await?;
    connection
        .object_server()
        .at(
            PATH,
            Service {
                pid,
                mode: mode.clone(),
            },
        )
        .await?;
    connection
        .object_server()
        .at(PATH, UnitState { mode, reads })
        .await?;
    connection.request_name("org.freedesktop.systemd1").await?;
    Ok(connection)
}
fn window(address: &str, pid: u32) -> Value {
    json!({"address":address,"class":"chrome-play.example.test__-Default","initialClass":"chrome-play.example.test__-Default","pid":pid,"workspace":{"id":9,"name":"9"}})
}
async fn applications(session: &Session) -> Result<Vec<Value>> {
    let result = call(&session.proxy().await?, "applications.query", json!({})).await?;
    Ok(result["data"]["applications"]["applications"]
        .as_array()
        .context("applications")?
        .clone())
}
async fn wait_owned(session: &Session, count: u64) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let apps = applications(session).await?;
            let app = apps
                .iter()
                .find(|app| app["id"] == "ok.desktop")
                .context("catalog app")?;
            if app["running_count"] == count {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("ownership did not reconcile")??;
    Ok(())
}

#[tokio::test]
async fn generated_wayland_identity_is_grouped_placed_and_recovered_after_daemon_restart()
-> Result<()> {
    let mut session = Session::start(true).await?;
    let mut app = tokio::process::Command::new(tool("sleep")?)
        .arg("60")
        .kill_on_drop(true)
        .spawn()?;
    let pid = app.id().context("app pid")?;
    let _manager = manager(&session, pid, Arc::new(AtomicUsize::new(0)), Arc::default()).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop","action":"launch","workspace_id":"4"}),
    )
    .await?;
    let id = accepted["data"]["operation"]["id"].as_str().unwrap();
    loop {
        if operation(&mut stream, id, "running")
            .await?
            .get("launch_backend")
            .is_some()
        {
            break;
        }
    }
    session
        .compositor
        .set(&json!([window("0x700", pid)]).to_string());
    assert_eq!(
        operation(&mut stream, id, "completed").await?["placement"]["status"],
        "placed"
    );
    wait_owned(&session, 1).await?;
    assert!(
        !applications(&session)
            .await?
            .iter()
            .any(|a| a["kind"] == "window-group")
    );
    drop(stream);
    drop(proxy);
    session.restart().await?;
    wait_owned(&session, 1).await?;
    // A different browser process with the very same generated class stays out.
    session
        .compositor
        .set(&json!([window("0x700", pid), window("0x800", std::process::id())]).to_string());
    call(&session.proxy().await?, "applications.refresh", json!({})).await?;
    let apps = applications(&session).await?;
    assert_eq!(
        apps.iter().find(|a| a["kind"] == "window-group").unwrap()["running_count"],
        1
    );
    // Losing the manager after recovery must not erase a still-live identity.
    drop(_manager);
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for action in ["activate", "close"] {
        let page = call(&proxy, "applications.query", json!({})).await?;
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({"target_id":"ok.desktop","action":action,"expected_revision":page["data"]["applications"]["revision"]}),
        )
        .await?;
        assert_eq!(accepted["ok"], true, "{accepted}");
        operation(
            &mut stream,
            accepted["data"]["operation"]["id"].as_str().unwrap(),
            "completed",
        )
        .await?;
    }
    let commands = session.compositor.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.contains("hl.dsp.window.close") && c.contains("0x700"))
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.contains("hl.dsp.window.") && c.contains("0x800"))
    );
    app.kill().await?;
    session
        .compositor
        .set(&json!([window("0x800", std::process::id())]).to_string());
    call(&proxy, "applications.refresh", json!({})).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let apps = applications(&session).await?;
            if apps.iter().find(|a| a["id"] == "ok.desktop").unwrap()["running"] == false {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("exited process retained ownership")??;
    drop(stream);
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn restart_recovery_rejects_historical_pids_inactive_units_and_changed_invocations()
-> Result<()> {
    let mut session = Session::start(true).await?;
    let mut app = tokio::process::Command::new(tool("sleep")?)
        .arg("60")
        .kill_on_drop(true)
        .spawn()?;
    let pid = app.id().unwrap();
    let mode = Arc::new(AtomicUsize::new(1));
    let reads = Arc::new(AtomicUsize::new(0));
    let _manager = manager(&session, pid, mode.clone(), reads.clone()).await?;
    session
        .compositor
        .set(&json!([window("0x900", pid)]).to_string());
    for invalid in [1, 2, 3] {
        mode.store(invalid, Ordering::SeqCst);
        session.restart().await?;
        let before = reads.load(Ordering::SeqCst);
        tokio::time::timeout(DEADLINE, async {
            loop {
                // Two reconciliation attempts avoid racing the tail of a read.
                let apps = applications(&session).await?;
                assert_eq!(
                    apps.iter().find(|a| a["id"] == "ok.desktop").unwrap()["running_count"],
                    0
                );
                if reads.load(Ordering::SeqCst) >= before + 4 {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .context("no recovery attempts")??;
    }
    mode.store(0, Ordering::SeqCst);
    wait_owned(&session, 1).await?;
    app.kill().await?;
    session.shutdown().await
}
