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
    let status = command(backend, "gtk-launch", [id])
        .stderr(Stdio::piped())
        .status()
        .await
        .context("start gtk-launch")?;
    anyhow::ensure!(status.success(), "desktop application launch failed");
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
    use super::LaunchBackend;

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
}
