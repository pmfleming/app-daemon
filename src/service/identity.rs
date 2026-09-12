//! Window ownership shared by actions, sampling, and query construction.
use std::collections::HashMap;

use crate::{
    catalog::Catalog,
    hyprland::{Client, Snapshot},
    resources::process_cgroup,
};

pub(super) fn group_windows<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
) -> HashMap<String, Vec<&'a Client>> {
    let mut grouped: HashMap<String, Vec<&Client>> = HashMap::new();
    for window in &windows.clients {
        grouped
            .entry(resolve_target(catalog, window))
            .or_default()
            .push(window);
    }
    grouped
}

pub(super) fn resolve_target(catalog: &Catalog, window: &Client) -> String {
    resolve_target_with_cgroup(catalog, window, process_cgroup(window.pid).as_deref())
}

pub(super) fn resolve_target_with_cgroup(
    catalog: &Catalog,
    window: &Client,
    cgroup: Option<&str>,
) -> String {
    cgroup
        .and_then(|path| cgroup_target(catalog, path))
        .or_else(|| window_classes(window).find_map(|class| exact_target(catalog, class)))
        .or_else(|| window_classes(window).find_map(|class| suffix_target(catalog, class)))
        .unwrap_or_else(|| {
            let class = if window.initial_class.is_empty() {
                &window.class
            } else {
                &window.initial_class
            };
            format!("window-group:{}", class.trim().to_ascii_lowercase())
        })
}

fn cgroup_target(catalog: &Catalog, path: &str) -> Option<String> {
    let decoded = systemd_unescape(path.rsplit('/').next()?);
    let base = if let Some(value) = decoded.strip_suffix(".scope") {
        let (base, token) = value.rsplit_once('-')?;
        is_instance_token(token).then_some(base)?
    } else if let Some(value) = decoded.strip_suffix(".service") {
        let (base, token) = value.rsplit_once('@')?;
        is_instance_token(token).then_some(base)?
    } else {
        return None;
    };
    catalog
        .entries
        .iter()
        .filter(|entry| !entry.launch_only)
        .filter(|entry| {
            let stem = entry.id.trim_end_matches(".desktop");
            base == stem || base.ends_with(&format!("-{stem}"))
        })
        .max_by_key(|entry| entry.id.len())
        .map(|entry| entry.id.clone())
}

fn is_instance_token(value: &str) -> bool {
    matches!(value.len(), 8 | 32) && value.chars().all(|character| character.is_ascii_hexdigit())
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

fn window_classes(window: &Client) -> impl Iterator<Item = &str> {
    [&window.class, &window.initial_class]
        .into_iter()
        .map(|class| class.trim().trim_end_matches(".desktop"))
}

fn exact_target(catalog: &Catalog, class: &str) -> Option<String> {
    catalog
        .entries
        .iter()
        .find(|entry| {
            !entry.launch_only
                && (entry
                    .id
                    .trim_end_matches(".desktop")
                    .eq_ignore_ascii_case(class)
                    || (!entry.startup_class.is_empty()
                        && entry.startup_class.eq_ignore_ascii_case(class)))
        })
        .map(|entry| entry.id.clone())
}

fn suffix_target(catalog: &Catalog, class: &str) -> Option<String> {
    let suffix = class.rsplit('.').next().unwrap_or_default();
    let mut matches = catalog.entries.iter().filter(|entry| {
        !entry.launch_only
            && entry
                .id
                .trim_end_matches(".desktop")
                .eq_ignore_ascii_case(suffix)
    });
    let target = matches.next()?;
    matches.next().is_none().then(|| target.id.clone())
}

pub(super) fn target_window<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    target_id: &str,
) -> Option<&'a Client> {
    windows
        .clients
        .iter()
        .find(|window| resolve_target(catalog, window) == target_id)
}
