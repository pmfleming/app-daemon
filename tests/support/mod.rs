use anyhow::{Context, Result};
use app_daemon::api::{BUS_NAME, INTERFACE, OBJECT_PATH};
use futures::StreamExt;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    process::{Child, Command},
    sync::broadcast,
    time::{sleep, timeout},
};
use zbus::{Connection, Proxy, proxy::SignalStream};

pub const DEADLINE: Duration = Duration::from_secs(20);

mod private_bus;

pub struct Session {
    pub root: tempfile::TempDir,
    pub connection: Connection,
    address: String,
    daemon: Child,
    daemon_command: Command,
    _bus: Child,
    pub compositor: MockCompositor,
}

impl Session {
    pub async fn start(uwsm: bool) -> Result<Self> {
        Self::start_with_workers(uwsm, 2).await
    }

    /// Configure one or two workers in the fake desktop fixture's child daemon,
    /// independently of the calling test's Tokio runtime.
    pub async fn start_with_workers(uwsm: bool, daemon_workers: usize) -> Result<Self> {
        anyhow::ensure!(
            (1..=2).contains(&daemon_workers),
            "private fixture worker bound"
        );
        // Keep Unix socket paths short even in a deeply nested Nix build directory.
        let root = tempfile::Builder::new()
            .prefix("ad-test-")
            .tempdir_in("/tmp")?;
        for dir in [
            "bin",
            "data/applications",
            "config",
            "state",
            "cache",
            "run",
            "empty",
        ] {
            fs::create_dir_all(root.path().join(dir))?;
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700))?;
        let shell = tool("sh")?;
        let sleeper = tool("sleep")?;
        executable(
            root.path(),
            "gtk-launch",
            &shell,
            &format!(
                "case \"$1\" in fail) printf 'fixture launch rejected' >&2; exit 42;; esac\n'{}' 60 </dev/null >/dev/null 2>&1 &\nprintf '%s' \"$!\" > \"$XDG_STATE_HOME/direct.pid\"\nexit 0\n",
                sleeper.display()
            ),
        )?;
        executable(
            root.path(),
            "xdg-terminal-exec",
            &shell,
            "printf '%s\\n' \"$@\" > \"$XDG_STATE_HOME/terminal-arguments\"\npwd > \"$XDG_STATE_HOME/working-directory\"\nexit 0\n",
        )?;
        if uwsm {
            executable(
                root.path(),
                "uwsm-app",
                &shell,
                &format!(
                    "printf '%s\\n' \"$@\" > \"$XDG_STATE_HOME/arguments\"\nfor arg do last=\"$arg\"; done\ncase \"$last\" in\nfail.desktop) printf 'fixture launch rejected' >&2; exit 17;;\nslow.desktop) printf '%s' \"$$\" > \"$XDG_STATE_HOME/handoff.pid\"; exec '{}' 60;;\nslow-peer.desktop) printf '%s' \"$$\" > \"$XDG_STATE_HOME/handoff-peer.pid\"; exec '{}' 60;;\nesac\n",
                    sleeper.display(),
                    sleeper.display()
                ),
            )?;
        } else {
            // Simulated manager verifies command routing/exit semantics. A separate
            // opt-in test exercises real systemd cgroup lifetime behavior.
            executable(
                root.path(),
                "systemd-run",
                &shell,
                "printf '%s\\n' \"$@\" > \"$XDG_STATE_HOME/arguments\"\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
            )?;
        }
        for id in ["ok", "fail", "slow"] {
            fs::write(
                root.path().join(format!("data/applications/{id}.desktop")),
                format!(
                    "[Desktop Entry]\nType=Application\nName={id}\nExec=true\nActions=inspect;\n[Desktop Action inspect]\nName=Inspect\nExec=true --inspect\n"
                ),
            )?;
        }
        fs::write(
            root.path()
                .join("data/applications/org.example.Bus-Only.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bus Only\nDBusActivatable=true\nActions=inspect;\n[Desktop Action inspect]\nName=Inspect\n",
        )?;
        let compositor = MockCompositor::start(root.path()).await?;
        let (bus, address) =
            private_bus::start(tool("dbus-daemon")?, root.path(), DEADLINE).await?;
        let connection = zbus::connection::Builder::address(address.as_str())?
            .build()
            .await?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_app-daemon"));
        command
            .arg("daemon")
            .env_clear()
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &address)
            .env("HOME", root.path())
            .env("PATH", root.path().join("bin"))
            .env("XDG_DATA_DIRS", root.path().join("empty"))
            .env(
                "APP_DAEMON_UPDATE_WORKER",
                "/nonexistent/fixture-update-worker",
            )
            .env("HYPRLAND_INSTANCE_SIGNATURE", "fixture")
            // Preserve the legacy standalone-launch qualification as rollback
            // coverage. workspace_journey exercises the coordinated Rust owner.
            .env("SHELLLIST_TEMPORARY_WORKSPACES", "0")
            .env("RUST_LOG", "error")
            .env("TOKIO_WORKER_THREADS", daemon_workers.to_string())
            .stdout(Stdio::null())
            .kill_on_drop(true);
        for (variable, dir) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "run"),
        ] {
            command.env(variable, root.path().join(dir));
        }
        // Keep instrumented children in cargo-llvm-cov's collection directory
        // without exposing the rest of the host environment to the fixture.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let daemon = command.spawn()?;
        let session = Self {
            root,
            connection,
            address,
            daemon,
            daemon_command: command,
            _bus: bus,
            compositor,
        };
        let dbus = zbus::fdo::DBusProxy::new(&session.connection).await?;
        timeout(DEADLINE, async {
            while !dbus.name_has_owner(BUS_NAME.try_into()?).await? {
                sleep(Duration::from_millis(10)).await;
            }
            Result::<()>::Ok(())
        })
        .await??;
        let response = call(&session.proxy().await?, "applications.refresh", json!({})).await?;
        anyhow::ensure!(response["ok"] == true, "refresh failed: {response}");
        Ok(session)
    }

    pub async fn restart(&mut self) -> Result<()> {
        self.shutdown().await?;
        self.start_again().await
    }

    pub async fn crash_restart(&mut self) -> Result<()> {
        // SIGKILL the private OS process: no graceful owner cleanup or unlink.
        self.daemon.kill().await?;
        self.start_again().await
    }

    async fn start_again(&mut self) -> Result<()> {
        self.daemon = self.daemon_command.spawn()?;
        let bus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        timeout(DEADLINE, async {
            while !bus.name_has_owner(BUS_NAME.try_into()?).await? {
                sleep(Duration::from_millis(10)).await;
            }
            Result::<()>::Ok(())
        })
        .await??;
        let response = call(&self.proxy().await?, "applications.refresh", json!({})).await?;
        anyhow::ensure!(response["ok"] == true, "refresh failed: {response}");
        Ok(())
    }

    pub fn bridge(&self) -> Result<Child> {
        Ok(Command::new(env!("CARGO_BIN_EXE_app-daemon"))
            .arg("client")
            .env_clear()
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("RUST_LOG", "error")
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?)
    }

    pub async fn proxy(&self) -> Result<Proxy<'_>> {
        Ok(Proxy::new(&self.connection, BUS_NAME, OBJECT_PATH, INTERFACE).await?)
    }
    pub async fn other_connection(&self) -> Result<Connection> {
        Ok(zbus::connection::Builder::address(self.address.as_str())?
            .build()
            .await?)
    }
    pub fn state_path(&self, name: &str) -> PathBuf {
        self.root.path().join("state").join(name)
    }
    pub async fn pid(&self, name: &str) -> Result<Pid> {
        timeout(DEADLINE, async {
            loop {
                if let Ok(text) = fs::read_to_string(self.state_path(name))
                    && let Ok(raw) = text.parse()
                    && let Some(pid) = Pid::from_raw(raw)
                {
                    return pid;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("fixture process did not start")
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        let pid = Pid::from_raw(self.daemon.id().context("daemon exited early")? as i32)
            .context("daemon pid")?;
        kill_process(pid, Signal::TERM)?;
        let status = timeout(DEADLINE, self.daemon.wait()).await??;
        anyhow::ensure!(status.success(), "daemon shutdown failed: {status}");
        self.validate_history_after_shutdown().await
    }

    pub async fn replace_compositor_session(&mut self) -> Result<()> {
        fs::rename(
            self.root.path().join("run/hypr/fixture"),
            self.root.path().join("run/hypr/replacement"),
        )?;
        self.compositor.disconnect();
        let status = timeout(DEADLINE, self.daemon.wait()).await??;
        anyhow::ensure!(!status.success(), "session loss must exit non-zero");
        self.validate_history_after_shutdown().await
    }

    async fn validate_history_after_shutdown(&self) -> Result<()> {
        let path = self.state_path("app-daemon/resource-history-turso-v1.db");
        anyhow::ensure!(path.is_file(), "history database missing after shutdown");
        let database = turso::Builder::new_local(path.to_str().context("history path")?)
            .with_io(turso::IoBackend::Syscall)
            .experimental_multiprocess_wal(true)
            .build()
            .await?;
        let connection = database.connect()?;
        let version = connection
            .query("PRAGMA user_version", ())
            .await?
            .next()
            .await?
            .context("history schema missing")?
            .get::<i64>(0)?;
        anyhow::ensure!(version == 1, "unexpected history schema");
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for name in ["handoff.pid", "handoff-peer.pid", "direct.pid"] {
            if let Ok(text) = fs::read_to_string(self.state_path(name))
                && let Ok(raw) = text.parse()
                && let Some(pid) = Pid::from_raw(raw)
                && fs::read(format!("/proc/{raw}/environ")).is_ok_and(|environment| {
                    let expected = format!(
                        "XDG_STATE_HOME={}",
                        self.root.path().join("state").display()
                    );
                    environment
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == expected.as_bytes())
                })
            {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
    }
}

pub struct MockCompositor {
    state: Arc<Mutex<CompositorState>>,
    events: broadcast::Sender<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct CompositorState {
    response: String,
    active_workspace: Value,
    commands: Vec<String>,
    move_behavior: (bool, bool),
    reject_focus: bool,
}

impl CompositorState {
    fn respond(&mut self, request: String) -> String {
        if request == "j/status" {
            return r#"{"configProvider":"lua"}"#.into();
        }
        if request == "j/version" {
            return r#"{"version":"0.57.1"}"#.into();
        }
        if request.starts_with("eval assert(type(") {
            return "ok".into();
        }
        let output = if self.reject_focus
            && (request.contains("hl.dsp.focus") || request.contains("focuswindow"))
        {
            "fixture rejected focus".into()
        } else if request == "j/activeworkspace" {
            self.active_workspace.to_string()
        } else if request == "j/clients" {
            self.response.clone()
        } else if request.contains("hl.dsp.window.move")
            || request.contains("movetoworkspacesilent")
        {
            let (accept, apply) = self.move_behavior;
            if accept && apply {
                apply_move(&mut self.response, &request);
            }
            if accept {
                "ok".into()
            } else {
                "fixture rejected move".into()
            }
        } else {
            "ok".into()
        };
        self.commands.push(request);
        output
    }
}
impl MockCompositor {
    async fn start(root: &Path) -> Result<Self> {
        let path = root.join("run/hypr/fixture");
        fs::create_dir_all(&path)?;
        let command = UnixListener::bind(path.join(".socket.sock"))?;
        let event = UnixListener::bind(path.join(".socket2.sock"))?;
        let state = Arc::new(Mutex::new(CompositorState {
            response: "[]".into(),
            active_workspace: json!({"id":1,"name":"1","address":"1","type":"normal"}),
            commands: Vec::new(),
            move_behavior: (true, true),
            reject_focus: false,
        }));
        let reply = Arc::clone(&state);
        let (events, _) = broadcast::channel::<String>(32);
        let sender = events.clone();
        let command_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = command.accept().await {
                let reply = Arc::clone(&reply);
                tokio::spawn(async move {
                    let mut request = String::new();
                    if stream.read_to_string(&mut request).await.is_ok() {
                        let output = reply.lock().unwrap().respond(request);
                        let _ = stream.write_all(output.as_bytes()).await;
                    }
                });
            }
        });
        let event_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = event.accept().await {
                let mut events = sender.subscribe();
                tokio::spawn(async move {
                    while let Ok(line) = events.recv().await {
                        if line == "disconnect"
                            || stream
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Ok(Self {
            state,
            events,
            tasks: vec![command_task, event_task],
        })
    }
    pub fn active_workspace(&self, workspace: Value) {
        self.state.lock().unwrap().active_workspace = workspace;
    }
    pub fn set(&self, response: &str) {
        self.state.lock().unwrap().response = response.to_owned();
        let _ = self.events.send("openwindow>>fixture".into());
    }
    pub fn disconnect(&self) {
        let _ = self.events.send("disconnect".into());
    }
    pub fn commands(&self) -> Vec<String> {
        self.state.lock().unwrap().commands.clone()
    }
    pub fn reject_focus(&self) {
        self.state.lock().unwrap().reject_focus = true;
    }
    pub fn moves(&self, accept: bool, apply: bool) {
        self.state.lock().unwrap().move_behavior = (accept, apply);
    }
}

fn apply_move(response: &mut String, request: &str) {
    let Some(workspace) = request.split("workspace = ").nth(1).and_then(|value| {
        value
            .get(1..)
            .and_then(|tail| tail.split(value.chars().next()?).next())
    }) else {
        return;
    };
    let Some(address) = request
        .split("window = 'address:")
        .nth(1)
        .and_then(|value| value.split('\'').next())
    else {
        return;
    };
    if let Ok(mut windows) = serde_json::from_str::<Vec<Value>>(response) {
        for window in &mut windows {
            if window["address"] == address {
                window["workspace"] = if let Ok(id) = workspace.parse::<i64>() {
                    json!({"id":id,"address":workspace,"name":workspace,"type":"normal"})
                } else {
                    let address = workspace.strip_prefix("name:").unwrap_or(workspace);
                    json!({"address":address,"name":address,"type":"normal"})
                };
            }
        }
        *response = serde_json::to_string(&windows).unwrap();
    }
}
impl Drop for MockCompositor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub async fn call(proxy: &Proxy<'_>, method: &str, params: Value) -> Result<Value> {
    raw_call(proxy, method, &params.to_string()).await
}
pub async fn raw_call(proxy: &Proxy<'_>, method: &str, params: &str) -> Result<Value> {
    let response: String = timeout(DEADLINE, proxy.call("Call", &(method, params))).await??;
    Ok(serde_json::from_str(&response)?)
}
pub async fn cancel(proxy: &Proxy<'_>, id: &str) -> Result<Value> {
    let response: String = timeout(DEADLINE, proxy.call("Cancel", &(id,))).await??;
    Ok(serde_json::from_str(&response)?)
}
pub async fn events(proxy: &Proxy<'_>) -> Result<SignalStream<'static>> {
    let events = proxy.receive_signal("Event").await?;
    let _: String = proxy
        .call("Subscribe", &(vec!["applications.operation"],))
        .await?;
    Ok(events)
}
pub async fn operation(events: &mut SignalStream<'_>, id: &str, status: &str) -> Result<Value> {
    timeout(DEADLINE, async {
        while let Some(message) = events.next().await {
            let (stream, body): (String, String) = message.body().deserialize()?;
            let event: Value = serde_json::from_str(&body)?;
            if event["data"]["operation"]["id"] == id
                && event["data"]["operation"]["status"] == status
            {
                assert_eq!(stream, "applications.operation");
                assert_eq!(event["stream"], stream);
                assert_eq!(event["protocol"], "app-api");
                assert_eq!(event["version"], 2);
                assert_eq!(event["event"], status);
                assert!(event["subscription_id"].is_string());
                assert!(event.get("operation").is_none());
                for private in ["unit", "provenance"] {
                    assert!(event["data"]["operation"].get(private).is_none());
                }
                return Ok(event["data"]["operation"].clone());
            }
        }
        anyhow::bail!("event stream closed")
    })
    .await
    .context("operation event timed out")?
}
pub fn running(pid: Pid) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_pid()))
        .is_ok_and(|stat| !stat.contains(") Z"))
}
pub async fn stopped(pid: Pid) -> Result<()> {
    timeout(DEADLINE, async {
        while running(pid) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("fixture child survived cancellation/shutdown")
}
pub fn tool(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .with_context(|| format!("integration tests require {name}"))
}
fn executable(root: &Path, name: &str, shell: &Path, body: &str) -> Result<()> {
    let path = root.join("bin").join(name);
    fs::write(&path, format!("#!{}\n{body}", shell.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
