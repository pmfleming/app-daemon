use std::collections::HashMap;

use crate::{
    catalog::{Catalog, CatalogEntry},
    hyprland::{self, Client, Snapshot},
    model::{
        ApplicationIdentity, ApplicationPage, ApplicationRuntime, ApplicationSummary, WindowSummary,
    },
    resources::{ResourceSnapshot, process_cgroup},
    service::QueryParams,
};

pub(super) fn combined_revision(catalog: &Catalog, windows: &Snapshot) -> u64 {
    catalog.revision.rotate_left(17) ^ windows.revision
}

pub(super) fn page(
    catalog: &Catalog,
    windows: &Snapshot,
    resources: &ResourceSnapshot,
    params: &QueryParams,
) -> ApplicationPage {
    let revision = combined_revision(catalog, windows);
    let available = windows.available;
    let mut grouped: HashMap<String, Vec<&Client>> = HashMap::new();
    for window in &windows.clients {
        grouped
            .entry(resolve_target(catalog, window))
            .or_default()
            .push(window);
    }

    let mut applications: Vec<ApplicationSummary> = catalog
        .entries
        .iter()
        .map(|entry| {
            summary_for_entry(
                entry,
                grouped.remove(&entry.id).unwrap_or_default(),
                resources,
                revision,
            )
        })
        .collect();
    applications.extend(
        grouped
            .into_iter()
            .map(|(id, clients)| summary_for_unmatched(id, clients, resources, revision)),
    );
    applications = applications
        .into_iter()
        .filter_map(|mut application| {
            let matched = search_match(&application, &params.query)?;
            application.match_score = matched.score;
            application.match_kind = matched.kind.into();
            application.score = if params.query.trim().is_empty() {
                application.runtime_score
            } else {
                matched
                    .score
                    .saturating_mul(100_000)
                    .saturating_add(application.runtime_score)
            };
            Some(application)
        })
        .collect();
    applications.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                left.identity
                    .name
                    .to_lowercase()
                    .cmp(&right.identity.name.to_lowercase())
            })
            .then_with(|| left.identity.id.cmp(&right.identity.id))
    });
    let limit = params.limit.clamp(1, 1000);
    let has_more = applications.len() > limit;
    applications.truncate(limit);
    ApplicationPage {
        revision,
        generation: params.generation,
        applications,
        has_more,
        hyprland_available: available,
    }
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
    let unit = path.rsplit('/').next()?;
    let decoded = systemd_unescape(unit);
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
    value.len() == 8 && value.chars().all(|character| character.is_ascii_hexdigit())
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

fn instances(
    target_id: &str,
    clients: &[&Client],
    resources: &ResourceSnapshot,
) -> Vec<WindowSummary> {
    clients
        .iter()
        .map(|window| {
            let usage = resources.usage_for_target(target_id, [window.pid]);
            WindowSummary {
                id: hyprland::window_id(&window.address),
                title: window.title.clone(),
                class: window.class.clone(),
                workspace_id: window.workspace.id.to_string(),
                workspace_name: window.workspace.name.clone(),
                focused: window.focus_rank == 0,
                focus_rank: window.focus_rank,
                resources: usage,
            }
        })
        .collect()
}

fn summary(
    identity: ApplicationIdentity,
    desktop_actions: Vec<crate::model::DesktopActionSummary>,
    clients: Vec<&Client>,
    resources: &ResourceSnapshot,
    revision: u64,
) -> ApplicationSummary {
    let usage = resources.usage_for_target(&identity.id, clients.iter().map(|window| window.pid));
    let instances = instances(&identity.id, &clients, resources);
    let focused = instances.iter().any(|window| window.focused);
    let best_rank = instances
        .iter()
        .map(|window| window.focus_rank)
        .min()
        .unwrap_or(i64::MAX);
    let runtime_score = running_score(focused, best_rank);
    ApplicationSummary {
        identity,
        revision,
        runtime: ApplicationRuntime {
            running: !instances.is_empty(),
            focused,
            running_count: instances.len(),
            resources: usage,
            instances,
        },
        desktop_actions,
        match_score: 0,
        match_kind: "none".into(),
        runtime_score,
        score: runtime_score,
    }
}

fn summary_for_entry(
    entry: &CatalogEntry,
    clients: Vec<&Client>,
    resources: &ResourceSnapshot,
    revision: u64,
) -> ApplicationSummary {
    let identity = ApplicationIdentity {
        id: entry.id.clone(),
        kind: entry.kind().into(),
        name: entry.name.clone(),
        generic_name: entry.generic_name.clone(),
        comment: entry.comment.clone(),
        icon: entry.icon.clone(),
        keywords: entry.keywords.clone(),
        categories: entry.categories.clone(),
        startup_class: entry.startup_class.clone(),
    };
    summary(
        identity,
        entry.actions.clone(),
        clients,
        resources,
        revision,
    )
}

fn summary_for_unmatched(
    id: String,
    clients: Vec<&Client>,
    resources: &ResourceSnapshot,
    revision: u64,
) -> ApplicationSummary {
    let name = clients
        .first()
        .filter(|window| !window.class.is_empty())
        .map_or("Untitled", |window| &window.class)
        .to_owned();
    let keywords = clients
        .iter()
        .flat_map(|window| [window.title.clone(), window.class.clone()])
        .collect();
    let identity = ApplicationIdentity {
        id,
        kind: "window-group".into(),
        name,
        generic_name: "Running window".into(),
        comment: String::new(),
        icon: String::new(),
        keywords,
        categories: Vec::new(),
        startup_class: String::new(),
    };
    summary(identity, Vec::new(), clients, resources, revision)
}

pub(super) fn running_score(focused: bool, focus_rank: i64) -> i64 {
    if focused {
        20_000
    } else if focus_rank != i64::MAX {
        10_000 + (1_000 - focus_rank).max(0)
    } else {
        0
    }
}

struct SearchMatch {
    score: i64,
    kind: &'static str,
}

fn search_match(application: &ApplicationSummary, query: &str) -> Option<SearchMatch> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(SearchMatch {
            score: 0,
            kind: "none",
        });
    }

    let name = application.identity.name.to_lowercase();
    let id = application.identity.id.to_lowercase();
    let id_stem = id.trim_end_matches(".desktop");
    let searchable = search_values(application).join(" ").to_lowercase();
    let acronym = search_acronym(application);
    let tokens = query.split_whitespace().collect::<Vec<_>>();
    if !tokens
        .iter()
        .all(|token| searchable.contains(token) || (token.len() <= 5 && acronym.contains(token)))
    {
        return None;
    }

    if name == query {
        return Some(ranked_match(12_000, name.len(), "exact-name"));
    }
    if id == query || id_stem == query {
        return Some(ranked_match(11_800, id.len(), "exact-id"));
    }
    if name.starts_with(&query) {
        return Some(ranked_match(11_500, name.len(), "name-prefix"));
    }
    if id.starts_with(&query) || id_stem.starts_with(&query) {
        return Some(ranked_match(11_000, id.len(), "id-prefix"));
    }
    if let Some(index) = name.find(&query) {
        return Some(SearchMatch {
            score: 9_500 - index as i64 * 10 - name.len().min(500) as i64,
            kind: "name-substring",
        });
    }
    if let Some(index) = id.find(&query) {
        return Some(SearchMatch {
            score: 9_000 - index as i64 * 10 - id.len().min(500) as i64,
            kind: "id-substring",
        });
    }
    if let Some(index) = searchable.find(&query) {
        return Some(SearchMatch {
            score: 7_500 - index.min(500) as i64,
            kind: "metadata",
        });
    }
    if query.len() <= 5
        && let Some(index) = acronym.find(&query)
    {
        return Some(SearchMatch {
            score: 6_500 - index as i64 * 10 - acronym.len().min(500) as i64,
            kind: "acronym",
        });
    }
    Some(SearchMatch {
        score: 5_000 - tokens.len() as i64,
        kind: "terms",
    })
}

fn ranked_match(base: i64, length: usize, kind: &'static str) -> SearchMatch {
    SearchMatch {
        score: base - length.min(500) as i64,
        kind,
    }
}

fn search_values(application: &ApplicationSummary) -> Vec<&str> {
    [
        application.identity.name.as_str(),
        application.identity.generic_name.as_str(),
        application.identity.comment.as_str(),
        application.identity.id.as_str(),
        application.identity.startup_class.as_str(),
    ]
    .into_iter()
    .chain(application.identity.keywords.iter().map(String::as_str))
    .chain(application.identity.categories.iter().map(String::as_str))
    .chain(
        application
            .runtime
            .instances
            .iter()
            .flat_map(|window| [window.title.as_str(), window.class.as_str()]),
    )
    .collect()
}

fn search_acronym(application: &ApplicationSummary) -> String {
    let mut acronym = String::new();
    for value in search_values(application) {
        let mut previous_alphanumeric = false;
        let mut previous_lowercase = false;
        for character in value.chars() {
            let boundary =
                !previous_alphanumeric || (character.is_ascii_uppercase() && previous_lowercase);
            if character.is_ascii_alphanumeric() && boundary {
                acronym.push(character.to_ascii_lowercase());
            }
            previous_alphanumeric = character.is_ascii_alphanumeric();
            previous_lowercase = character.is_ascii_lowercase();
        }
    }
    acronym
}
