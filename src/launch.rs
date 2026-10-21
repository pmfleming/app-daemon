use std::{os::unix::fs::FileExt, process::Stdio, time::Duration};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{catalog::CatalogEntry, platform::command_available};

mod provenance;
pub(crate) use provenance::Provenance;

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
    #[serde(skip)]
    provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<crate::model::WorkspacePlacement>,
}

impl LaunchReceipt {
    pub(crate) fn ownership_roots(&self) -> impl Iterator<Item = crate::process::Identity> + '_ {
        self.provenance.roots()
    }

    pub(crate) fn owns_process(&self, pid: u32) -> bool {
        self.provenance.owns(pid)
            || crate::process::process_cgroup(pid).is_some_and(|path| self.owns_cgroup(&path))
    }

    pub(crate) fn remember_process(&mut self, pid: u32) {
        self.provenance.remember(pid);
    }

    pub(crate) fn include_existing(&mut self, existing: Provenance) {
        self.provenance.merge(existing);
    }

    pub(crate) fn observe_processes(&mut self) {
        self.provenance.observe(self.unit.as_deref());
    }

    async fn track_unit(mut self) -> Self {
        if let Some(unit) = self.unit.as_deref()
            && unit.ends_with(".service")
            && let Some(pid) = provenance::unit_main_pid(unit).await
        {
            self.remember_process(pid);
        }
        self.observe_processes();
        self
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
            provenance: Provenance::default(),
            placement: None,
        }
    }
}

/// Keep desktop activation, fallback and terminal routing at the launch boundary.
pub(crate) async fn launch_entry(
    entry: &CatalogEntry,
    action: Option<&str>,
) -> anyhow::Result<LaunchReceipt> {
    anyhow::ensure!(
        action.is_none_or(|id| entry.actions.iter().any(|action| action.id == id)),
        "desktop action is unavailable"
    );
    if let Some(receipt) = activate_dbus_entry(entry, action).await? {
        return Ok(receipt);
    }
    let command = match action {
        Some(action) => {
            let command = entry.parse_action(action)?;
            if LaunchBackend::detect() == LaunchBackend::Uwsm {
                return launch_desktop_action(&entry.id, action).await;
            }
            command
        }
        None if !entry.requires_terminal() || LaunchBackend::detect() == LaunchBackend::Uwsm => {
            return launch_desktop(&entry.id).await;
        }
        None => entry.launch_command()?,
    };
    launch_entry_command(entry, &command).await
}

async fn activate_dbus_entry(
    entry: &CatalogEntry,
    action: Option<&str>,
) -> anyhow::Result<Option<LaunchReceipt>> {
    if !entry.dbus_activatable() {
        return Ok(None);
    }
    match activate_dbus(&entry.id, action).await {
        Ok(receipt) => Ok(Some(receipt)),
        Err(error) => {
            let fallback = action.map_or_else(
                || entry.launch_command(),
                |action| entry.parse_action(action),
            );
            if fallback.is_err() {
                return Err(error.context("activate desktop entry without an Exec fallback"));
            }
            tracing::debug!(target_id = %entry.id, %error, "D-Bus activation failed; using desktop Exec fallback");
            Ok(None)
        }
    }
}

async fn launch_entry_command(
    entry: &CatalogEntry,
    command: &[String],
) -> anyhow::Result<LaunchReceipt> {
    let (program, arguments) = command
        .split_first()
        .context("desktop application command is empty")?;
    if entry.requires_terminal() {
        let arguments = std::iter::once("--").chain(command.iter().map(String::as_str));
        return spawn_for_application(
            &entry.id,
            "xdg-terminal-exec",
            arguments,
            entry.working_directory(),
        )
        .await
        .context("start application in the default terminal");
    }
    spawn_for_application(&entry.id, program, arguments, entry.working_directory())
        .await
        .context("start desktop action")
}

const LAUNCH_HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

async fn activate_dbus(id: &str, action: Option<&str>) -> anyhow::Result<LaunchReceipt> {
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
        let mut receipt = LaunchReceipt {
            backend: "dbus-activation".into(),
            scope: "session-bus".into(),
            ..LaunchBackend::Direct.into()
        };
        if let Some(pid) = provenance::bus_owner_pid(&connection, name.as_str()).await {
            receipt.remember_process(pid);
        }
        receipt.observe_processes();
        Ok::<_, anyhow::Error>(receipt)
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
    }
    .track_unit()
    .await)
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
    }
    .track_unit()
    .await)
}

async fn checked_handoff(command: Command, description: &str) -> anyhow::Result<()> {
    checked_handoff_with_timeout(command, description, LAUNCH_HANDOFF_TIMEOUT).await
}

async fn checked_handoff_with_timeout(
    mut command: Command,
    description: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    // An unlinked file stays writable through inherited descriptors even after
    // daemon shutdown. A pipe drainer would still expose descendants to SIGPIPE.
    let diagnostics = tempfile::tempfile().context("create launcher diagnostic sink")?;
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics.try_clone()?))
        .spawn()
        .with_context(|| format!("start {description}"))?;
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .with_context(|| format!("{description} launch handoff timed out"))?
        .with_context(|| format!("wait for {description} launcher"))?;
    if status.success() {
        return Ok(());
    }
    // Do not change the shared write offset, or wait for descendant EOF.
    let mut detail = [0_u8; 8192];
    let count = diagnostics.read_at(&mut detail, 0).unwrap_or(0);
    let detail = String::from_utf8_lossy(&detail[..count]).trim().to_owned();
    anyhow::bail!("{description} launch failed ({status}): {detail}")
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
    let mut receipt = LaunchReceipt {
        unit,
        ..backend.into()
    };
    if backend == LaunchBackend::Direct {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start application command {program}"))?;
        if let Some(pid) = child.id() {
            receipt.remember_process(pid);
        }
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    } else {
        checked_handoff(command, "application command").await?;
    }
    Ok(receipt.track_unit().await)
}

fn ensure_safe_backend(backend: LaunchBackend) -> anyhow::Result<()> {
    let cgroup = crate::process::process_cgroup(std::process::id());
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
