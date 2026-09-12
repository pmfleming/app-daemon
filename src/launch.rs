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
    Direct,
}

impl LaunchBackend {
    pub fn detect() -> Self {
        Self::detect_with(command_available)
    }

    fn detect_with(available: impl FnOnce(&str) -> bool) -> Self {
        if available("uwsm-app") {
            Self::Uwsm
        } else {
            Self::Direct
        }
    }

    const fn description(self) -> (&'static str, &'static str) {
        match self {
            Self::Uwsm => ("uwsm-app", "app-graphical.slice"),
            Self::Direct => ("direct", "inherited"),
        }
    }

    pub const fn name(self) -> &'static str {
        self.description().0
    }

    pub const fn scope(self) -> &'static str {
        self.description().1
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

impl From<LaunchBackend> for LaunchReceipt {
    fn from(backend: LaunchBackend) -> Self {
        Self {
            backend: backend.name().into(),
            scope: backend.scope().into(),
            unit: None,
        }
    }
}

const LAUNCH_HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn launch_desktop(id: &str) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    let mut command = desktop_command(backend, id);
    let unit = (backend == LaunchBackend::Uwsm).then(|| application_unit(id));
    if let Some(unit) = &unit {
        // Insert the override before the command's `--` delimiter.
        command = uwsm_desktop_command(id, unit);
    }
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
    checked_handoff(
        command(backend, &target, std::iter::empty::<&str>()),
        "desktop action",
    )
    .await?;
    Ok(backend.into())
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

pub fn spawn(
    program: &str,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    let mut child = command(backend, program, arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start application command {program}"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(backend.into())
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

fn desktop_command(backend: LaunchBackend, id: &str) -> Command {
    match backend {
        // uwsm-app is the fast, drop-in client for `uwsm app`. Passing the
        // desktop ID lets UWSM honor Terminal, Path, and other entry metadata.
        LaunchBackend::Uwsm => command(backend, id, std::iter::empty::<&str>()),
        LaunchBackend::Direct => command(backend, "gtk-launch", [id.trim_end_matches(".desktop")]),
    }
}

fn command(
    backend: LaunchBackend,
    program: &str,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Command {
    let mut command = match backend {
        LaunchBackend::Uwsm => {
            let mut command = Command::new("uwsm-app");
            // A scope-mode systemd-run remains attached to foreground applications.
            // Service mode returns once exec succeeds, making operation completion a
            // launch handoff rather than an application-lifetime notification.
            command.args(["-t", "service", "--"]).arg(program);
            command
        }
        LaunchBackend::Direct => Command::new(program),
    };
    command.args(arguments);
    command
}
