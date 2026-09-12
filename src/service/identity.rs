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
        .and_then(|path| catalog.target_for_cgroup(path))
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
