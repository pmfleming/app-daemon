use super::Catalog;

impl Catalog {
    /// Resolve only explicitly application-named units, including nested cgroups.
    /// Prefer the most specific desktop ID; never guess from a generic service.
    pub(crate) fn target_for_cgroup(&self, path: &str) -> Option<String> {
        self.application_cgroup(path).map(|(target, _)| target)
    }

    pub(crate) fn application_cgroup<'a>(&self, mut path: &'a str) -> Option<(String, &'a str)> {
        loop {
            if let Some(target) = self.target_for_unit(path.rsplit('/').next()?) {
                return Some((target, path));
            }
            path = path.rsplit_once('/')?.0;
        }
    }

    fn target_for_unit(&self, unit: &str) -> Option<String> {
        if unit == "app-daemon.service" {
            return None;
        }
        let decoded = systemd_unescape(unit);
        let base = if let Some(value) = decoded.strip_suffix(".scope") {
            let (base, token) = value.rsplit_once('-')?;
            instance_token(token).then_some(base)?
        } else if let Some(value) = decoded.strip_suffix(".service") {
            match value.rsplit_once('@') {
                Some((base, token)) => instance_token(token).then_some(base)?,
                None => value,
            }
        } else {
            return None;
        };
        if !base.starts_with("app-") {
            // D-Bus activatable applications may install a stable user service
            // named after their bus ID instead of a generated app-* unit.
            return self
                .entries
                .iter()
                .find(|entry| {
                    !entry.launch_only
                        && entry.dbus_activatable()
                        && (base == entry.id.trim_end_matches(".desktop")
                            || base.strip_prefix("dbus-")
                                == Some(entry.id.trim_end_matches(".desktop")))
                })
                .map(|entry| entry.id.clone());
        }
        self.entries
            .iter()
            .filter(|entry| !entry.launch_only)
            .filter(|entry| {
                let stem = entry.id.trim_end_matches(".desktop");
                base.strip_prefix("app-") == Some(stem) || base.ends_with(&format!("-{stem}"))
            })
            .max_by_key(|entry| entry.id.len())
            .map(|entry| entry.id.clone())
    }
}

fn instance_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn systemd_unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index..index + 2) == Some(b"\\x")
            && let Some(hex) = bytes.get(index + 2..index + 4)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_background_application_units_but_not_generic_services() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("org.example.App.desktop"),
            "[Desktop Entry]\nType=Application\nName=App\nExec=true\n",
        )?;
        let catalog = Catalog::from_paths(vec![dir.path().into()]);
        for path in [
            "/user.slice/app-org.example.App@12345678.service",
            "/user.slice/app-org.example.App@12345678901234567890123456789012.service/child",
            "/user.slice/app-flatpak-org.example.App-433952237.scope",
            "/user.slice/app-dbus-org.example.App.service",
            "/user.slice/app-org.example.App.service",
        ] {
            assert_eq!(
                catalog.target_for_cgroup(path).as_deref(),
                Some("org.example.App.desktop"),
                "{path}"
            );
        }
        for path in [
            "/app-daemon.service",
            "/user@1000.service",
            "/org.example.App.service",
            "/app-unknown.service",
            "/app-org.example.App.scope",
        ] {
            assert!(catalog.target_for_cgroup(path).is_none(), "{path}");
        }
        Ok(())
    }
}
