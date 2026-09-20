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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, Command},
    sync::broadcast,
    time::{sleep, timeout},
};
use zbus::{Connection, Proxy, proxy::SignalStream};

pub const DEADLINE: Duration = Duration::from_secs(20);

pub struct Session {
    pub root: tempfile::TempDir,
    pub connection: Connection,
    address: String,
    daemon: Child,
    _bus: Child,
    pub compositor: MockCompositor,
}

impl Session {
    pub async fn start(uwsm: bool) -> Result<Self> {
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
                    "printf '%s\\n' \"$@\" > \"$XDG_STATE_HOME/arguments\"\nfor arg do last=\"$arg\"; done\ncase \"$last\" in\nfail.desktop) printf 'fixture launch rejected' >&2; exit 17;;\nslow.desktop) printf '%s' \"$$\" > \"$XDG_STATE_HOME/handoff.pid\"; exec '{}' 60;;\nesac\n",
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
        // Do not use --session: it loads the host's session.conf, unavailable in
        // some sandboxes and capable of activating real desktop services.
        let config = root.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
                root.path().join("run").display()
            ),
        )?;
        let mut bus = Command::new(tool("dbus-daemon")?)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let address = timeout(
            DEADLINE,
            BufReader::new(bus.stdout.take().context("bus stdout")?)
                .lines()
                .next_line(),
        )
        .await??
        .context("private bus address")?;
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
            .env("HYPRLAND_INSTANCE_SIGNATURE", "fixture")
            .env("RUST_LOG", "error")
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
        anyhow::ensure!(
            timeout(DEADLINE, self.daemon.wait()).await??.success(),
            "daemon shutdown failed"
        );
        let file: Value = serde_json::from_slice(&fs::read(
            self.state_path("app-daemon/resource-history-v1.json"),
        )?)?;
        anyhow::ensure!(file["version"] == 1, "final history snapshot missing");
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for name in ["handoff.pid", "direct.pid"] {
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
    response: Arc<Mutex<String>>,
    events: broadcast::Sender<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl MockCompositor {
    async fn start(root: &Path) -> Result<Self> {
        let path = root.join("run/hypr/fixture");
        fs::create_dir_all(&path)?;
        let command = UnixListener::bind(path.join(".socket.sock"))?;
        let event = UnixListener::bind(path.join(".socket2.sock"))?;
        let response = Arc::new(Mutex::new("[]".to_owned()));
        let reply = Arc::clone(&response);
        let (events, _) = broadcast::channel::<String>(32);
        let sender = events.clone();
        let commands = tokio::spawn(async move {
            while let Ok((mut stream, _)) = command.accept().await {
                let reply = Arc::clone(&reply);
                tokio::spawn(async move {
                    let mut request = String::new();
                    if stream.read_to_string(&mut request).await.is_ok() {
                        let output = if request == "j/clients" {
                            reply.lock().unwrap().clone()
                        } else {
                            "ok".into()
                        };
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
            response,
            events,
            tasks: vec![commands, event_task],
        })
    }
    pub fn set(&self, response: &str) {
        *self.response.lock().unwrap() = response.to_owned();
        let _ = self.events.send("openwindow>>fixture".into());
    }
    pub fn disconnect(&self) {
        let _ = self.events.send("disconnect".into());
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
            let (_, body): (String, String) = message.body().deserialize()?;
            let event: Value = serde_json::from_str(&body)?;
            if event["operation"]["id"] == id && event["operation"]["status"] == status {
                return Ok(event["operation"].clone());
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
fn tool(name: &str) -> Result<PathBuf> {
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
