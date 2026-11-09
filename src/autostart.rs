//! XDG autostart files are the sole authority; no parallel database preference.
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, ensure};
use freedesktop_desktop_entry::DesktopEntry;
use shelllist_daemon_core::{XdgRoot, resolve_xdg_root};

pub(crate) fn directories() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = resolve_xdg_root(XdgRoot::Config) {
        paths.push(home.join("autostart"));
    }
    let system = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    paths.extend(
        std::env::split_paths(&system)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("autostart")),
    );
    paths
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    let mut parts = Path::new(id).components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(_)))
            && parts.next().is_none()
            && id.ends_with(".desktop"),
        "invalid autostart desktop identity"
    );
    Ok(())
}

fn existing(directories: &[PathBuf], id: &str) -> anyhow::Result<Option<String>> {
    validate_id(id)?;
    for directory in directories {
        match fs::read_to_string(directory.join(id)) {
            Ok(text) => return Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A broken override must not expose a lower-priority entry.
                if directory.join(id).symlink_metadata().is_ok() {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn parse(id: &str, text: &str) -> anyhow::Result<DesktopEntry> {
    let entry = DesktopEntry::from_str(Path::new(id), text, None::<&[String]>)?;
    ensure!(
        entry.groups.0.contains_key("Desktop Entry"),
        "invalid autostart desktop entry"
    );
    Ok(entry)
}

pub(crate) fn configured(id: &str) -> Option<bool> {
    configured_in(&directories(), id).ok()
}

fn configured_in(directories: &[PathBuf], id: &str) -> anyhow::Result<bool> {
    let Some(text) = existing(directories, id)? else {
        return Ok(false);
    };
    let entry = parse(id, &text)?;
    Ok(!entry.hidden() && entry.desktop_entry("X-GNOME-Autostart-enabled") != Some("false"))
}

pub(crate) fn set(id: &str, source: &Path, enabled: bool) -> anyhow::Result<()> {
    let home = resolve_xdg_root(XdgRoot::Config)
        .context("autostart configuration directory unavailable")?
        .join("autostart");
    set_in(&home, &directories(), id, source, enabled)
}

fn set_in(
    home: &Path,
    directories: &[PathBuf],
    id: &str,
    source: &Path,
    enabled: bool,
) -> anyhow::Result<()> {
    validate_id(id)?;
    let target = home.join(id);
    if let Ok(metadata) = target.symlink_metadata() {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "autostart entry is externally managed; edit its source configuration"
        );
    }
    let mut text = match existing(directories, id)? {
        Some(text) => text,
        None => fs::read_to_string(source).context("application desktop entry unavailable")?,
    };
    let entry = parse(id, &text)?;
    // A user Hidden=true mask may contain no launch metadata at all.
    if enabled && entry.desktop_entry("Exec").is_none() && !entry.dbus_activatable() {
        text = fs::read_to_string(source)?;
        parse(id, &text)?;
    }
    let mut output = String::new();
    let mut main = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            main = trimmed == "[Desktop Entry]";
            output.push_str(line);
            output.push('\n');
            if main {
                output.push_str(if enabled {
                    "Hidden=false\nX-GNOME-Autostart-enabled=true\n"
                } else {
                    "Hidden=true\nX-GNOME-Autostart-enabled=false\n"
                });
            }
            continue;
        }
        if main
            && trimmed.split_once('=').is_some_and(|(key, _)| {
                matches!(key.trim(), "Hidden" | "X-GNOME-Autostart-enabled")
            })
        {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    fs::create_dir_all(home)?;
    let mut staged = tempfile::NamedTempFile::new_in(home)?;
    staged.write_all(output.as_bytes())?;
    staged.as_file().sync_all()?;
    staged.persist(&target)?;
    fs::File::open(home)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_rejects_shortcuts_missing_targets_and_mixed_patches() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("shortcut.desktop"),
            "[Desktop Entry]\nType=Application\nName=Shortcut\nExec=true\nX-Shelllist-LaunchOnly=true\n",
        )?;
        let service = crate::service::ApplicationService::with_fixture_catalog(
            crate::catalog::Catalog::from_paths(vec![temp.path().into()]),
        )
        .await;
        for (id, category) in [
            ("shortcut.desktop", None),
            ("missing.desktop", None),
            ("shortcut.desktop", Some(Some("code".into()))),
        ] {
            assert!(
                service
                    .update_settings(crate::service::UpdateSettingsParams {
                        target_id: id.into(),
                        category,
                        new_workspace: None,
                        autostart: Some(true),
                    })
                    .await
                    .is_err()
            );
        }
        service.shutdown().await;
        Ok(())
    }

    #[test]
    fn toggles_preserve_launch_metadata_and_override_system_entries() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("user");
        let system = temp.path().join("system");
        fs::create_dir(&system)?;
        let source = temp.path().join("app.desktop");
        let original = "[Desktop Entry]\nType=Application\nName=App\nExec=app --flag %U\nOnlyShowIn=Test;\nTryExec=app\nTerminal=true\nPath=/tmp\nActions=open;\n[Desktop Action open]\nName=Open\nExec=app --open\n";
        fs::write(&source, original)?;
        fs::write(system.join("app.desktop"), original)?;
        let paths = [home.clone(), system.clone()];
        assert!(configured_in(&paths, "app.desktop")?);
        set_in(&home, &paths, "app.desktop", &source, false)?;
        assert!(!configured_in(&paths, "app.desktop")?);
        assert_eq!(fs::read_to_string(system.join("app.desktop"))?, original);
        set_in(&home, &paths, "app.desktop", &source, true)?;
        assert!(configured_in(&paths, "app.desktop")?);
        let enabled = fs::read_to_string(home.join("app.desktop"))?;
        for line in original.lines() {
            assert!(enabled.lines().any(|value| value == line));
        }
        set_in(&home, &paths, "app.desktop", &source, true)?;
        assert_eq!(fs::read_to_string(home.join("app.desktop"))?, enabled);
        fs::write(home.join("app.desktop"), "[Desktop Entry]\nHidden=true\n")?;
        set_in(&home, &paths, "app.desktop", &source, true)?;
        assert!(fs::read_to_string(home.join("app.desktop"))?.contains("Exec=app --flag %U"));
        Ok(())
    }

    #[test]
    fn fresh_entries_failures_and_managed_files() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("autostart");
        let source = temp.path().join("app.desktop");
        fs::write(
            &source,
            "[Desktop Entry]\nType=Application\nName=App\nExec=app\n",
        )?;
        let paths = [home.clone()];
        assert!(!configured_in(&paths, "app.desktop")?);
        assert!(set_in(&home, &paths, "../app.desktop", &source, true).is_err());
        set_in(&home, &paths, "app.desktop", &source, true)?;
        assert!(configured_in(&paths, "app.desktop")?);
        fs::remove_file(home.join("app.desktop"))?;
        std::os::unix::fs::symlink(&source, home.join("app.desktop"))?;
        assert!(set_in(&home, &paths, "app.desktop", &source, false).is_err());
        assert!(!fs::read_to_string(&source)?.contains("Hidden"));
        fs::remove_file(home.join("app.desktop"))?;
        fs::write(home.join("app.desktop"), "invalid")?;
        assert!(configured_in(&paths, "app.desktop").is_err());
        assert!(set_in(&home, &paths, "app.desktop", &source, true).is_err());
        assert_eq!(fs::read_to_string(home.join("app.desktop"))?, "invalid");
        Ok(())
    }
}
