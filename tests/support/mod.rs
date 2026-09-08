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
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
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
}

impl Session {
    pub async fn start(uwsm: bool) -> Result<Self> {
        let root = tempfile::tempdir()?;
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
        let sleep = tool("sleep")?;
        executable(root.path(), "hyprctl", &shell, "printf '[]\\n'\n")?;
        executable(
            root.path(),
            "gtk-launch",
            &shell,
            &format!(
                "printf '%s' \"$$\" > \"$XDG_STATE_HOME/direct.pid\"\nexec '{}' 60\n",
                sleep.display()
            ),
        )?;
        if uwsm {
            executable(
                root.path(),
                "uwsm-app",
                &shell,
                &format!(
                    "printf '%s\\n' \"$@\" > \"$XDG_STATE_HOME/arguments\"\ncase \"$4\" in\n  fail.desktop) printf 'fixture launch rejected' >&2; exit 17 ;;\n  slow.desktop) printf '%s' \"$$\" > \"$XDG_STATE_HOME/handoff.pid\"; exec '{}' 60 ;;\nesac\n",
                    sleep.display()
                ),
            )?;
        }
        for id in ["ok", "fail", "slow"] {
            fs::write(
                root.path().join(format!("data/applications/{id}.desktop")),
                format!(
                    "[Desktop Entry]\nType=Application\nName={id}\nExec=true\nActions=inspect;\n\n[Desktop Action inspect]\nName=Inspect\nExec=true --inspect\n"
                ),
            )?;
        }
        let mut bus = Command::new(tool("dbus-daemon")?)
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
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
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("HOME", root.path())
            .env("PATH", root.path().join("bin"))
            .env("XDG_DATA_DIRS", root.path().join("empty"))
            .env("HYPRLAND_INSTANCE_SIGNATURE", "app-daemon-integration-test")
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
        let daemon = command.spawn()?;
        let session = Self {
            root,
            connection,
            address,
            daemon,
            _bus: bus,
        };
        let dbus = zbus::fdo::DBusProxy::new(&session.connection).await?;
        timeout(DEADLINE, async {
            while !dbus.name_has_owner(BUS_NAME.try_into()?).await? {
                tokio::time::sleep(Duration::from_millis(10)).await;
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
        // Direct launches intentionally outlive the daemon; fixtures must not.
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
