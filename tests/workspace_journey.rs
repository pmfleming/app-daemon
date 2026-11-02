//! Opt-in real-daemon journey. All mutations use a private compositor and bus.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shelllist_hyprland::{Client, workspace};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone)]
struct Activation {
    root: PathBuf,
    display: String,
    signature: String,
    weston: PathBuf,
    children: std::sync::Arc<std::sync::Mutex<Vec<Process>>>,
    delayed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[zbus::interface(name = "org.freedesktop.Application")]
impl Activation {
    fn activate(&self, _platform: std::collections::HashMap<String, zbus::zvariant::OwnedValue>) {
        if self.delayed.load(std::sync::atomic::Ordering::Relaxed) {
            let activation = self.clone();
            fs::write(self.root.join("handoff"), "accepted").unwrap();
            tokio::spawn(async move {
                for _ in 0..1000 {
                    if activation.root.join("release-late-window").exists() {
                        activation.open();
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        } else {
            self.open();
        }
    }
}
impl Activation {
    fn open(&self) {
        let child = spawn(
            &self.weston.join("bin/weston-flower"),
            &[],
            &self.root,
            &self.display,
            &self.signature,
            "flower.log",
        );
        self.children.lock().unwrap().push(child);
    }
}
fn spawn(
    binary: &Path,
    args: &[&str],
    root: &Path,
    display: &str,
    signature: &str,
    log: &str,
) -> Process {
    let file = fs::File::create(root.join(log)).unwrap();
    Process(
        Command::new(binary)
            .args(args)
            .env_clear()
            .env("XDG_RUNTIME_DIR", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_DATA_DIRS", root.join("none"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_SESSION_TYPE", "wayland")
            .env("PATH", root.join("bin"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", root.display()),
            )
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                format!("unix:path={}/no-system-bus", root.display()),
            )
            .env("WAYLAND_DISPLAY", display)
            .env("HYPRLAND_INSTANCE_SIGNATURE", signature)
            .env("HYPRLAND_NO_SD_NOTIFY", "1")
            .env("AQ_DRM_DEVICES", "/dev/null")
            .env("LIBSEAT_BACKEND", "noop")
            .env_remove("DISPLAY")
            .env_remove("NOTIFY_SOCKET")
            .env_remove("SHELLLIST_TEMPORARY_WORKSPACES")
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap(),
    )
}
async fn call(bus: &zbus::Connection, service: &str, method: &str, params: Value) -> Result<Value> {
    let proxy = zbus::Proxy::new(
        bus,
        format!("org.laufan.{service}Daemon"),
        format!("/org/laufan/{service}Daemon"),
        format!("org.laufan.{service}Daemon2"),
    )
    .await?;
    let reply: String = tokio::time::timeout(
        Duration::from_secs(20),
        proxy.call("Call", &(method, params.to_string())),
    )
    .await??;
    let value: Value = serde_json::from_str(&reply)?;
    ensure!(value["ok"] == true, "{method}: {value}");
    Ok(value["data"].clone())
}
async fn ready(bus: &zbus::Connection, service: &str) {
    let dbus = zbus::fdo::DBusProxy::new(bus).await.unwrap();
    for _ in 0..150 {
        if dbus
            .name_has_owner(format!("org.laufan.{service}Daemon").try_into().unwrap())
            .await
            .unwrap_or(false)
        {
            let proxy = zbus::Proxy::new(
                bus,
                format!("org.laufan.{service}Daemon"),
                format!("/org/laufan/{service}Daemon"),
                "org.freedesktop.DBus.Introspectable",
            )
            .await
            .unwrap();
            if let Ok(Ok(xml)) = tokio::time::timeout(
                Duration::from_millis(500),
                proxy.call::<_, _, String>("Introspect", &()),
            )
            .await
                && xml.contains(&format!("org.laufan.{service}Daemon2"))
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{service} daemon did not claim its private bus name");
}
async fn terminal(bus: &zbus::Connection, id: &str) -> Value {
    for _ in 0..150 {
        let response = match call(
            bus,
            "App",
            "applications.operation.status",
            json!({"operation_id":id}),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let operation = &response["operation_status"];
        if !matches!(operation["status"].as_str(), Some("accepted" | "running")) {
            return operation.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("operation did not settle");
}

#[tokio::test]
#[ignore = "requires WORKSPACE_TEST_PARENT_DISPLAY, WORKSPACE_TEST_BAR, and installed compositor/test client"]
async fn real_daemons_launch_recover_and_close_on_private_compositor() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("wsj-")
        .tempdir_in("/tmp")?
        .keep();
    eprintln!("private daemon journey directory: {}", root.display());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    for directory in ["config", "cache", "data/applications", "bin"] {
        fs::create_dir_all(root.join(directory))?;
    }
    let parent = std::env::var("WORKSPACE_TEST_PARENT_DISPLAY")?;
    let hyprland = PathBuf::from(std::env::var("WORKSPACE_TEST_HYPRLAND")?);
    let bar_binary = PathBuf::from(std::env::var("WORKSPACE_TEST_BAR")?);
    let weston = PathBuf::from(std::env::var("WORKSPACE_TEST_WESTON")?);
    let dbus_binary = PathBuf::from(std::env::var("WORKSPACE_TEST_DBUS")?);
    let config = root.join("config/hyprland.lua");
    fs::write(
        &config,
        "-- Private daemon acceptance; no startup commands.\n",
    )?;
    let mut compositor = spawn(
        &hyprland,
        &["--config", config.to_str().unwrap()],
        &root,
        &parent,
        "",
        "hyprland.log",
    );
    let mut transport = None;
    for _ in 0..150 {
        ensure!(
            compositor.0.try_wait()?.is_none(),
            "compositor exited; logs in {}",
            root.display()
        );
        if let Ok(entries) = fs::read_dir(root.join("hypr")) {
            for entry in entries.flatten() {
                let client = Client::new(
                    root.clone(),
                    Some(entry.file_name().to_string_lossy().into_owned()),
                );
                if workspace::snapshot(&client).await.is_ok() {
                    transport = Some(client);
                    break;
                }
            }
        }
        if transport.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let transport = transport.context("private compositor socket")?;
    let snapshot = workspace::snapshot(&transport).await?;
    let signature = snapshot.session;
    let display = fs::read_dir(&root)?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        .context("private Wayland socket")?;
    let bus_address = format!("unix:path={}/bus", root.display());
    let _dbus = spawn(
        &dbus_binary,
        &["--session", "--nofork", "--address", &bus_address],
        &root,
        &display,
        &signature,
        "dbus.log",
    );
    for _ in 0..100 {
        if root.join("bus").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let children = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let delayed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let activation = Activation {
        root: root.clone(),
        display: display.clone(),
        signature: signature.clone(),
        weston: weston.clone(),
        children: children.clone(),
        delayed: delayed.clone(),
    };
    let bus = zbus::connection::Builder::address(bus_address.as_str())?
        .name("org.example.WorkspaceFixture")?
        .serve_at("/org/example/WorkspaceFixture", activation)?
        .build()
        .await?;
    fs::write(
        root.join("data/applications/org.example.WorkspaceFixture.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Workspace fixture\nDBusActivatable=true\nExec={}/bin/weston-flower\nStartupWMClass=org.freedesktop.weston.flower\nCategories=Development;\n",
            weston.display()
        ),
    )?;
    let mut bar = spawn(
        &bar_binary,
        &["daemon"],
        &root,
        &display,
        &signature,
        "bar.log",
    );
    ready(&bus, "Bar").await;
    eprintln!(
        "workspace context: {}",
        call(&bus, "Bar", "workspace.context", json!({})).await?
    );
    let app_binary = PathBuf::from(env!("CARGO_BIN_EXE_app-daemon"));
    let mut app = spawn(
        &app_binary,
        &["daemon"],
        &root,
        &display,
        &signature,
        "app.log",
    );
    ready(&bus, "App").await;
    let page = call(
        &bus,
        "App",
        "applications.refresh",
        json!({"query":"Workspace fixture"}),
    )
    .await?;
    ensure!(
        page["applications"]["applications"]
            .as_array()
            .is_some_and(|rows| rows
                .iter()
                .any(|r| r["id"] == "org.example.WorkspaceFixture.desktop")),
        "fixture catalog: {page}"
    );
    let params = json!({"target_id":"org.example.WorkspaceFixture.desktop", "action":"launch", "launch_target":"new", "request_id":"journey-new"});
    let id = "operation-journey-new";
    match call(&bus, "App", "applications.execute", params.clone()).await {
        Ok(accepted) => ensure!(
            accepted["operation"]["id"] == id,
            "unexpected operation identity"
        ),
        Err(error) => ensure!(
            error.to_string().contains("outcome-unknown"),
            "admission failed: {error}"
        ),
    }
    let result = terminal(&bus, id).await;
    ensure!(
        result["launch_backend"].is_string() && result["placement"]["status"] == "placed",
        "launch/placement: {result}; logs {}",
        root.display()
    );
    let destination: i64 = result["placement"]["workspace_id"]
        .as_str()
        .unwrap()
        .parse()?;
    ensure!(destination > 5, "temporary allocation");
    let before = workspace::snapshot(&transport).await?;
    ensure!(
        before.windows.iter().any(|w| w.workspace.id == destination),
        "window not placed"
    );
    // Lost terminal/admission replies resolve to the same operation, without launch.
    ensure!(
        call(&bus, "App", "applications.execute", params.clone()).await?["operation"]["id"] == id,
        "duplicate request changed operation"
    );
    app.0.kill()?;
    app.0.wait()?;
    app = spawn(
        &app_binary,
        &["daemon"],
        &root,
        &display,
        &signature,
        "app-restarted.log",
    );
    ready(&bus, "App").await;
    ensure!(
        call(
            &bus,
            "App",
            "applications.operation.status",
            json!({"operation_id":id})
        )
        .await?["operation_status"]["id"]
            == id,
        "durable status lost"
    );
    ensure!(
        call(&bus, "App", "applications.execute", params).await?["operation"]["id"] == id,
        "restart replayed launch"
    );
    ensure!(
        workspace::snapshot(&transport).await?.windows.len() == before.windows.len(),
        "duplicate launch window"
    );
    bar.0.kill()?;
    bar.0.wait()?;
    bar = spawn(
        &bar_binary,
        &["daemon"],
        &root,
        &display,
        &signature,
        "bar-restarted.log",
    );
    ready(&bus, "Bar").await;
    let created = call(
        &bus,
        "Bar",
        "workspace.command",
        json!({"command":"create", "request_id":"journey-add"}),
    )
    .await?;
    ensure!(
        created["workspace_operation"]["lease"]["workspace_id"]
            .as_i64()
            .unwrap()
            > destination,
        "restart reused allocation"
    );
    let current = workspace::snapshot(&transport).await?;
    let monitor = current.monitors.iter().find(|m| m.focused).unwrap();
    workspace::execute(
        &transport,
        &workspace::focus_local(
            &monitor.name,
            monitor.active_workspace.id,
            destination,
            true,
        )?,
    )
    .await?;
    call(
        &bus,
        "Bar",
        "workspace.command",
        json!({"command":"close", "request_id":"journey-close"}),
    )
    .await?;
    for _ in 0..100 {
        if workspace::snapshot(&transport).await?.windows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(
        workspace::snapshot(&transport).await?.windows.is_empty(),
        "normal workspace close did not finish"
    );
    // Crash after checked activation, while the new window is deliberately late.
    delayed.store(true, std::sync::atomic::Ordering::Relaxed);
    let late = json!({"target_id":"org.example.WorkspaceFixture.desktop", "action":"launch", "launch_target":"new", "request_id":"journey-late"});
    let _ = call(&bus, "App", "applications.execute", late.clone()).await;
    for _ in 0..100 {
        if root.join("handoff").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ensure!(
        root.join("handoff").exists(),
        "delayed activation never handed off"
    );
    let reservation = call(
        &bus,
        "Bar",
        "workspace.operation.get",
        json!({"session":signature,"request_id":"operation-journey-late"}),
    )
    .await?;
    let late_destination = reservation["workspace_operation"]["lease"]["workspace_id"]
        .as_i64()
        .unwrap();
    app.0.kill()?;
    app.0.wait()?;
    app = spawn(
        &app_binary,
        &["daemon"],
        &root,
        &display,
        &signature,
        "app-crash-recovery.log",
    );
    ready(&bus, "App").await;
    let recovered = terminal(&bus, "operation-journey-late").await;
    ensure!(
        recovered["status"] == "unknown",
        "unfinished launch must recover unknown: {recovered}"
    );
    ensure!(
        call(&bus, "App", "applications.execute", late).await?["operation"]["status"] == "unknown",
        "unknown launch replayed"
    );
    fs::write(root.join("release-late-window"), "release")?;
    for _ in 0..100 {
        if !workspace::snapshot(&transport).await?.windows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after = workspace::snapshot(&transport).await?;
    ensure!(
        after.windows.len() == 1 && after.windows[0].workspace.id != late_destination,
        "late window revived retired placement"
    );
    ensure!(
        children.lock().unwrap().len() == 2,
        "recovery duplicated activation"
    );
    call(
        &bus,
        "Bar",
        "workspace.command",
        json!({"command":"close", "request_id":"journey-close-late"}),
    )
    .await?;
    if let (Ok(qml), Ok(fixture), Ok(plugins)) = (
        std::env::var("WORKSPACE_TEST_QML"),
        std::env::var("WORKSPACE_TEST_VETO_FIXTURE"),
        std::env::var("WORKSPACE_TEST_QT_PLUGINS"),
    ) {
        for _ in 0..100 {
            if workspace::snapshot(&transport).await?.windows.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let log = fs::File::create(root.join("veto.log"))?;
        let imports = Path::new(&qml)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("lib/qt-6/qml");
        let _veto = Process(
            Command::new(qml)
                .arg("-I")
                .arg(imports)
                .arg(fixture)
                .env_clear()
                .env("HOME", &root)
                .env("XDG_RUNTIME_DIR", &root)
                .env("WAYLAND_DISPLAY", &display)
                .env("QT_QPA_PLATFORM", "wayland")
                .env("QT_QUICK_BACKEND", "software")
                .env("QT_PLUGIN_PATH", plugins)
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()?,
        );
        for _ in 0..100 {
            if workspace::snapshot(&transport).await?.windows.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ensure!(
            workspace::snapshot(&transport).await?.windows.len() == 1,
            "veto fixture did not map"
        );
        call(
            &bus,
            "Bar",
            "workspace.command",
            json!({"command":"close", "request_id":"journey-veto"}),
        )
        .await?;
        for _ in 0..100 {
            let status = call(
                &bus,
                "Bar",
                "workspace.operation.get",
                json!({"session":signature,"request_id":"journey-veto"}),
            )
            .await?;
            if status["workspace_operation"]["status"] == "still-open" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ensure!(
            workspace::snapshot(&transport).await?.windows.len() == 2,
            "close killed refusing client or its new dialog"
        );
        let status = call(
            &bus,
            "Bar",
            "workspace.operation.get",
            json!({"session":signature,"request_id":"journey-veto"}),
        )
        .await?;
        ensure!(
            status["workspace_operation"]["status"] == "still-open",
            "close did not retain honest refusal state"
        );
    }
    drop(app);
    drop(bar);
    eprintln!("private daemon journey evidence: {}", root.display());
    Ok(())
}
