//! Opt-in, real user-manager test. Creates only uniquely named temporary units.
use anyhow::{Context, Result};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const FIXTURE_ENV: &str = "APP_DAEMON_LIFETIME_FIXTURE";

#[test]
#[ignore = "requires a real user systemd manager; run --test systemd_lifetime --ignored --exact isolated_application_survives_host_service_stop"]
fn isolated_application_survives_host_service_stop() -> Result<()> {
    lifetime(false)
}

#[test]
#[ignore = "requires a real user systemd manager"]
fn isolated_desktop_handoff_survives_host_service_stop() -> Result<()> {
    lifetime(true)
}

fn lifetime(desktop: bool) -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("ad-life-")
        .tempdir_in("/tmp")?;
    let systemd_run = tool("systemd-run")?;
    let systemctl = tool("systemctl")?;
    let shell = tool("sh")?;
    let sleeper = tool("sleep")?;
    fs::create_dir(root.path().join("bin"))?;
    symlink(&systemd_run, root.path().join("bin/systemd-run"))?;
    let application = root.path().join("application");
    fs::write(
        &application,
        format!(
            "#!{}\nprintf '%s' \"$$\" > \"$APP_DAEMON_LIFETIME_FIXTURE/app.pid\"\nexec '{}' 60\n",
            shell.display(),
            sleeper.display()
        ),
    )?;
    fs::set_permissions(&application, fs::Permissions::from_mode(0o755))?;
    let gtk = root.path().join("bin/gtk-launch");
    fs::write(
        &gtk,
        format!(
            "#!{}\n'{}' </dev/null >/dev/null 2>&1 &\nexit 0\n",
            shell.display(),
            application.display()
        ),
    )?;
    fs::set_permissions(gtk, fs::Permissions::from_mode(0o755))?;
    let host_unit = format!(
        "app-daemon-review-host-{}.service",
        uuid::Uuid::new_v4().simple()
    );
    let mut cleanup = Units {
        systemctl,
        names: vec![host_unit.clone()],
    };
    let status = Command::new(systemd_run)
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--service-type=exec",
            "--unit",
            &host_unit,
        ])
        .arg(format!("--setenv={FIXTURE_ENV}={}", root.path().display()))
        .arg(format!("--setenv=APP_DAEMON_LIFETIME_DESKTOP={desktop}"))
        .arg(format!(
            "--setenv=PATH={}",
            root.path().join("bin").display()
        ))
        .arg("--")
        .arg(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "systemd_fixture_child",
            "--nocapture",
        ])
        .status()?;
    anyhow::ensure!(
        status.success(),
        "start fixture host service (a working user manager is required)"
    );
    let raw = wait_for(&root.path().join("app.pid"))?.parse::<u32>()?;
    let cgroup = fs::read_to_string(format!("/proc/{raw}/cgroup"))?;
    let unit = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .and_then(|path| path.rsplit('/').next())
        .context("application cgroup unit")?
        .to_owned();
    anyhow::ensure!(
        unit.contains("ad-life-")
            && unit.ends_with(if desktop { ".scope" } else { ".service" })
            && unit != host_unit,
        "application was not isolated: {cgroup}"
    );
    cleanup.names.push(unit);
    wait_for(&root.path().join("ready"))?;
    anyhow::ensure!(
        Command::new(&cleanup.systemctl)
            .args(["--user", "stop", &host_unit])
            .status()?
            .success(),
        "stop fixture host service"
    );
    cleanup.names.retain(|unit| unit != &host_unit);
    let stat = fs::read_to_string(format!("/proc/{raw}/stat"))
        .context("application died with host service")?;
    anyhow::ensure!(!stat.contains(") Z"), "application died with host service");
    Ok(())
}

#[tokio::test]
#[ignore = "internal helper for the opt-in lifetime test"]
async fn systemd_fixture_child() -> Result<()> {
    let Some(root) = std::env::var_os(FIXTURE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    let application = root.join("application");
    let receipt = if std::env::var("APP_DAEMON_LIFETIME_DESKTOP").as_deref() == Ok("true") {
        let id = format!(
            "org.example.{}.desktop",
            root.file_name().unwrap().to_string_lossy()
        );
        app_daemon::launch::launch_desktop(&id).await?
    } else {
        app_daemon::launch::spawn(
            application.to_str().context("fixture path")?,
            std::iter::empty::<&str>(),
        )
        .await?
    };
    anyhow::ensure!(
        receipt.backend == "systemd-run",
        "fixture must exercise systemd fallback"
    );
    fs::write(root.join("ready"), "ready")?;
    std::future::pending::<()>().await;
    Ok(())
}

struct Units {
    systemctl: PathBuf,
    names: Vec<String>,
}
impl Drop for Units {
    fn drop(&mut self) {
        let _ = Command::new(&self.systemctl)
            .args(["--user", "stop"])
            .args(&self.names)
            .status();
    }
}
fn wait_for(path: &Path) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && !text.is_empty()
        {
            return Ok(text);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "fixture did not publish {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn tool(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|root| root.join(name))
        .find(|path| path.is_file())
        .with_context(|| format!("missing {name}"))
}
