//! Shared no-activation bus fixture for app unit and process qualification.
use anyhow::{Context, Result, ensure};
use std::{ffi::OsStr, os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::{Duration, timeout},
};

/// Start only a private bus: no host session.conf, service dirs or activation helper.
/// The returned kill-on-drop child owns the fixture lifetime, including errors.
pub(crate) async fn start(
    program: impl AsRef<OsStr>,
    root: &Path,
    deadline: Duration,
) -> Result<(Child, String)> {
    let runtime = root.join("run");
    std::fs::create_dir_all(&runtime)?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let config = root.join("bus.conf");
    std::fs::write(
        &config,
        format!(
            "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
            runtime.display()
        ),
    )?;
    let mut child = Command::new(program)
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let address = timeout(
        deadline,
        BufReader::new(child.stdout.take().context("private bus stdout")?)
            .lines()
            .next_line(),
    )
    .await??
    .context("private bus address")?;
    // Check the running bus, not just its XML: adding service directories must
    // fail the fixture before any app-daemon request could activate a service.
    timeout(deadline, async {
        let connection = zbus::connection::Builder::address(address.as_str())?
            .build()
            .await?;
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        let names = bus.list_activatable_names().await?;
        ensure!(
            names
                .iter()
                .all(|name| name.as_str() == "org.freedesktop.DBus"),
            "private fixture unexpectedly permits service activation"
        );
        Result::<()>::Ok(())
    })
    .await??;
    Ok((child, address))
}
