use std::{process::Stdio, time::Duration};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command};

use crate::platform::command_available;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchBackend {
    Uwsm,
    Systemd,
    Direct,
}

impl LaunchBackend {
    pub fn detect() -> Self {
        Self::detect_with(command_available)
    }

    fn detect_with(available: impl Fn(&str) -> bool) -> Self {
        if available("uwsm-app") {
            Self::Uwsm
        } else if available("systemd-run") {
            Self::Systemd
        } else {
            Self::Direct
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchReceipt {
    pub backend: String,
    pub scope: String,
    /// Exact unit used for this launch; never infer ownership from application class alone.
    #[serde(skip)]
    pub(crate) unit: Option<String>,
}

impl LaunchReceipt {
    pub(crate) fn owns_process(&self, pid: u32) -> bool {
        crate::resources::process_cgroup(pid).is_some_and(|path| self.owns_cgroup(&path))
    }

    fn owns_cgroup(&self, path: &str) -> bool {
        self.unit
            .as_deref()
            .is_some_and(|unit| path.split('/').any(|part| part == unit))
    }
}

impl From<LaunchBackend> for LaunchReceipt {
    fn from(backend: LaunchBackend) -> Self {
        let (backend, scope) = match backend {
            LaunchBackend::Uwsm => ("uwsm-app", "app-graphical.slice"),
            LaunchBackend::Systemd => ("systemd-run", "app-graphical.slice"),
            LaunchBackend::Direct => ("direct", "inherited"),
        };
        Self {
            backend: backend.into(),
            scope: scope.into(),
            unit: None,
        }
    }
}

const LAUNCH_HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn activate_dbus(id: &str, action: Option<&str>) -> anyhow::Result<LaunchReceipt> {
    tokio::time::timeout(LAUNCH_HANDOFF_TIMEOUT, async {
        let (name, path) = activation_address(id)?;
        let connection = zbus::Connection::session()
            .await
            .context("connect for desktop activation")?;
        let proxy = zbus::Proxy::new(
            &connection,
            name.to_owned(),
            path,
            "org.freedesktop.Application",
        )
        .await?;
        let startup = std::env::var("DESKTOP_STARTUP_ID").ok();
        let token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
        let mut platform = std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        if let Some(startup) = &startup {
            platform.insert("desktop-startup-id", startup.as_str().into());
        }
        if let Some(token) = &token {
            platform.insert("activation-token", token.as_str().into());
        }
        if let Some(action) = action {
            proxy
                .call::<_, _, ()>(
                    "ActivateAction",
                    &(action, Vec::<zbus::zvariant::Value<'_>>::new(), platform),
                )
                .await?;
        } else {
            proxy.call::<_, _, ()>("Activate", &(platform,)).await?;
        }
        Ok::<_, anyhow::Error>(LaunchReceipt {
            backend: "dbus-activation".into(),
            scope: "session-bus".into(),
            unit: None,
        })
    })
    .await
    .context("desktop D-Bus activation timed out")?
}

fn activation_address(
    id: &str,
) -> anyhow::Result<(
    zbus::names::WellKnownName<'_>,
    zbus::zvariant::OwnedObjectPath,
)> {
    let name = id
        .strip_suffix(".desktop")
        .context("D-Bus desktop ID must end in .desktop")?;
    let destination =
        zbus::names::WellKnownName::try_from(name).context("invalid D-Bus application name")?;
    let path = format!("/{}", name.replace('.', "/").replace('-', "_"));
    Ok((
        destination,
        zbus::zvariant::OwnedObjectPath::try_from(path)?,
    ))
}

pub async fn launch_desktop(id: &str) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    ensure_safe_backend(backend)?;
    let (command, unit) = desktop_command(backend, id);
    checked_handoff(command, "desktop application").await?;
    Ok(LaunchReceipt {
        unit,
        ..backend.into()
    })
}

pub async fn launch_desktop_action(id: &str, action_id: &str) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    anyhow::ensure!(backend == LaunchBackend::Uwsm, "UWSM is unavailable");
    let target = format!("{id}:{action_id}");
    let unit = application_unit(id);
    checked_handoff(uwsm_desktop_command(&target, &unit), "desktop action").await?;
    Ok(LaunchReceipt {
        unit: Some(unit),
        ..backend.into()
    })
}

async fn checked_handoff(command: Command, description: &str) -> anyhow::Result<()> {
    checked_handoff_with_timeout(command, description, LAUNCH_HANDOFF_TIMEOUT).await
}

async fn checked_handoff_with_timeout(
    mut command: Command,
    description: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {description}"))?;
    let mut stderr = child
        .stderr
        .take()
        .context("capture launcher diagnostics")?;
    let mut detail = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut open = true;
    let status = tokio::time::timeout(timeout, async {
        loop {
            tokio::select! {
                status = child.wait() => return status,
                read = stderr.read(&mut buffer), if open => {
                    match read {
                        Ok(0) | Err(_) => open = false,
                        Ok(count) => capture_diagnostic(&mut detail, &buffer[..count]),
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!("{description} launch handoff timed out"))?
    .with_context(|| format!("wait for {description} launcher"))?;
    // GTK may pass stderr to an application. Observe the launcher's exit, not
    // descendant EOF, and never retain unbounded diagnostics from a noisy helper.
    let _ = tokio::time::timeout(Duration::from_millis(20), async {
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            capture_diagnostic(&mut detail, &buffer[..count]);
        }
    })
    .await;
    if status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&detail).trim().to_owned();
    anyhow::bail!("{description} launch failed ({status}): {detail}")
}

fn capture_diagnostic(detail: &mut Vec<u8>, bytes: &[u8]) {
    const MAX_DIAGNOSTIC: usize = 8192;
    detail
        .extend_from_slice(&bytes[..bytes.len().min(MAX_DIAGNOSTIC.saturating_sub(detail.len()))]);
}

pub async fn spawn(
    program: &str,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> anyhow::Result<LaunchReceipt> {
    spawn_for_application(program, program, arguments, None).await
}

pub(crate) async fn spawn_for_application(
    target_id: &str,
    program: &str,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    directory: Option<&str>,
) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    ensure_safe_backend(backend)?;
    let unit = (backend != LaunchBackend::Direct).then(|| application_unit(target_id));
    let mut command = match backend {
        LaunchBackend::Uwsm => uwsm_desktop_command(program, unit.as_deref().unwrap_or_default()),
        LaunchBackend::Systemd => {
            systemd_command(program, unit.as_deref().unwrap_or_default(), false)
        }
        LaunchBackend::Direct => Command::new(program),
    };
    if let Some(directory) = directory.filter(|value| !value.is_empty()) {
        command.current_dir(directory);
    }
    command.args(arguments);
    if backend == LaunchBackend::Direct {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start application command {program}"))?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    } else {
        checked_handoff(command, "application command").await?;
    }
    Ok(LaunchReceipt {
        unit,
        ..backend.into()
    })
}

fn ensure_safe_backend(backend: LaunchBackend) -> anyhow::Result<()> {
    let cgroup = crate::resources::process_cgroup(std::process::id());
    anyhow::ensure!(
        backend != LaunchBackend::Direct
            || cgroup.as_deref().is_some_and(|path| !service_cgroup(path)),
        "safe launch isolation is unavailable; install uwsm-app or systemd-run (refusing to launch inside the daemon's service cgroup)"
    );
    Ok(())
}

fn service_cgroup(path: &str) -> bool {
    path.split('/').any(|part| part.ends_with(".service"))
}

pub(crate) fn application_unit(id: &str) -> String {
    let mut escaped = String::new();
    for byte in id.trim_end_matches(".desktop").bytes() {
        if byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(escaped, "\\x{byte:02x}");
        }
    }
    format!("app-{escaped}@{}.service", uuid::Uuid::new_v4().simple())
}

fn uwsm_desktop_command(id: &str, unit: &str) -> Command {
    let mut command = Command::new("uwsm-app");
    command.args(["-t", "service", "-u", unit, "--", id]);
    command
}

fn desktop_command(backend: LaunchBackend, id: &str) -> (Command, Option<String>) {
    match backend {
        LaunchBackend::Uwsm => {
            let unit = application_unit(id);
            (uwsm_desktop_command(id, &unit), Some(unit))
        }
        LaunchBackend::Systemd => {
            let unit = application_unit(id)
                .trim_end_matches(".service")
                .replace('@', "-")
                + ".scope";
            let mut command = systemd_command("gtk-launch", &unit, true);
            command.arg(id.trim_end_matches(".desktop"));
            (command, Some(unit))
        }
        LaunchBackend::Direct => {
            let mut command = Command::new("gtk-launch");
            command.arg(id.trim_end_matches(".desktop"));
            (command, None)
        }
    }
}

fn systemd_command(program: &str, unit: &str, launcher: bool) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--quiet",
        "--collect",
        "--unit",
        unit,
        "--slice=app-graphical.slice",
    ]);
    if launcher {
        // Scope mode waits for gtk-launch itself, NOT the application's lifetime.
        // Its descendants remain in an independently managed scope after handoff.
        command.arg("--scope");
    } else {
        // Direct application executables may run forever. Service exec handoff
        // reports exec failure without waiting for application termination.
        command.args([
            "--service-type=exec",
            "--property=ExitType=cgroup",
            "--same-dir",
        ]);
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str().filter(|name| !name.contains('=')) {
                command.arg(format!("--setenv={name}"));
            }
        }
    }
    command.arg("--").arg(program);
    command
}
