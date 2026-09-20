use std::borrow::Cow;

use super::Catalog;

impl Catalog {
    /// Resolve only explicitly application-named units, including nested cgroups.
    /// Prefer the most specific desktop ID; never guess from a generic service.
    pub(crate) fn target_for_cgroup(&self, path: &str) -> Option<&str> {
        self.application_cgroup(path).map(|(target, _)| target)
    }

    pub(crate) fn application_cgroup<'a>(&self, mut path: &'a str) -> Option<(&str, &'a str)> {
        loop {
            if let Some(target) = self.target_for_unit(path.rsplit('/').next()?) {
                return Some((target, path));
            }
            path = path.rsplit_once('/')?.0;
        }
    }

    fn target_for_unit(&self, unit: &str) -> Option<&str> {
        if unit == "app-daemon.service" {
            return None;
        }
        let decoded = systemd_unescape(unit);
        let base = unit_base(&decoded)?;
        let mut entries = self.entries.iter().filter(|entry| !entry.launch_only);
        let entry = if let Some(app) = base.strip_prefix("app-") {
            entries
                .filter(|entry| {
                    app.strip_suffix(entry.id.trim_end_matches(".desktop"))
                        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('-'))
                })
                .max_by_key(|entry| entry.id.len())
        } else {
            // Stable D-Bus services need an exact bus ID, not a suffix guess.
            entries.find(|entry| {
                let stem = entry.id.trim_end_matches(".desktop");
                entry.dbus_activatable()
                    && (base == stem || base.strip_prefix("dbus-") == Some(stem))
            })
        };
        entry.map(|entry| entry.id.as_str())
    }
}

fn unit_base(unit: &str) -> Option<&str> {
    let (base, token) = if let Some(scope) = unit.strip_suffix(".scope") {
        scope.rsplit_once('-')?
    } else {
        let service = unit.strip_suffix(".service")?;
        let Some(instance) = service.rsplit_once('@') else {
            return Some(service);
        };
        instance
    };
    instance_token(token).then_some(base)
}

fn instance_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn systemd_unescape(value: &str) -> Cow<'_, str> {
    if !value.contains("\\x") {
        return Cow::Borrowed(value);
    }
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
    Cow::Owned(String::from_utf8_lossy(&decoded).into_owned())
}
