use std::process::Stdio;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::platform::command_available;

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
}

impl From<LaunchBackend> for LaunchReceipt {
    fn from(backend: LaunchBackend) -> Self {
        Self {
            backend: backend.name().into(),
            scope: backend.scope().into(),
        }
    }
}

pub async fn launch_desktop(id: &str) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    let mut command = desktop_command(backend, id);
    let status = command
        .stderr(Stdio::piped())
        .status()
        .await
        .context("start desktop application")?;
    anyhow::ensure!(status.success(), "desktop application launch failed");
    Ok(backend.into())
}

pub async fn launch_desktop_action(id: &str, action_id: &str) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    anyhow::ensure!(backend == LaunchBackend::Uwsm, "UWSM is unavailable");
    let target = format!("{id}:{action_id}");
    let status = command(backend, &target, std::iter::empty::<&str>())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("start desktop action through UWSM")?;
    anyhow::ensure!(status.success(), "desktop action launch failed");
    Ok(backend.into())
}

pub fn spawn(
    program: &str,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> anyhow::Result<LaunchReceipt> {
    let backend = LaunchBackend::detect();
    command(backend, program, arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start application command {program}"))?;
    Ok(backend.into())
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
            command.arg("--").arg(program);
            command
        }
        LaunchBackend::Direct => Command::new(program),
    };
    command.args(arguments);
    command
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{LaunchBackend, command, desktop_command};

    #[test]
    fn prefers_uwsm_when_available() {
        assert_eq!(
            LaunchBackend::detect_with(|command| command == "uwsm-app"),
            LaunchBackend::Uwsm
        );
        assert_eq!(LaunchBackend::detect_with(|_| false), LaunchBackend::Direct);
    }

    #[test]
    fn describes_launch_isolation() {
        assert_eq!(LaunchBackend::Uwsm.name(), "uwsm-app");
        assert_eq!(LaunchBackend::Uwsm.scope(), "app-graphical.slice");
        assert_eq!(LaunchBackend::Direct.scope(), "inherited");
    }

    #[test]
    fn invokes_uwsm_with_an_argument_separator() {
        let command = command(LaunchBackend::Uwsm, "org.example.App.desktop", ["--new"]);
        assert_eq!(command.as_std().get_program(), OsStr::new("uwsm-app"));
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["--", "org.example.App.desktop", "--new"]
                .map(OsStr::new)
                .as_slice()
        );
    }

    #[test]
    fn passes_desktop_ids_to_uwsm_and_gtk_launch() {
        let uwsm = desktop_command(LaunchBackend::Uwsm, "org.example.App.desktop");
        assert_eq!(
            uwsm.as_std().get_args().collect::<Vec<_>>(),
            ["--", "org.example.App.desktop"].map(OsStr::new).as_slice()
        );

        let direct = desktop_command(LaunchBackend::Direct, "org.example.App.desktop");
        assert_eq!(direct.as_std().get_program(), OsStr::new("gtk-launch"));
        assert_eq!(
            direct.as_std().get_args().collect::<Vec<_>>(),
            [OsStr::new("org.example.App")]
        );
    }
}
