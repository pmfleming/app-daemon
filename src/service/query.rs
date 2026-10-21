use std::{cmp::Reverse, collections::HashMap};

use serde::{Deserialize, Serialize};

use crate::{
    catalog::{Catalog, CatalogEntry},
    hyprland::{self, Client, Snapshot},
    model::{
        ApplicationIdentity, ApplicationPage, ApplicationRuntime, ApplicationSummary, WindowSummary,
    },
    resources::ResourceSnapshot,
    settings::{ApplicationSettings, SettingsStore, inferred_category},
};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct QueryParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub generation: u64,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    500
}

pub(super) fn combined_revision(
    catalog: &Catalog,
    windows: &Snapshot,
    settings_revision: u64,
    runtime_revision: u64,
) -> u64 {
    // Revisions cross JSON into QML's JavaScript runtime. Keep this opaque hash
    // exactly representable as a Number so an unchanged revision can be sent
    // back in expected_revision without being rounded.
    crate::metrics::json_revision(
        catalog.revision.rotate_left(17)
            ^ windows.revision
            ^ settings_revision.rotate_left(31)
            ^ runtime_revision.rotate_left(7),
    )
}

/// Action precondition for one application, not the whole desktop. Keep launch
/// metadata, preferences and window identity/placement; exclude presentation-only
/// titles, focus order and sampled resources. Grouping already verifies ownership.
pub(super) fn target_revision(
    target_id: &str,
    entry: Option<&CatalogEntry>,
    clients: &[&Client],
    settings: Option<&ApplicationSettings>,
    available: bool,
) -> u64 {
    let mut windows = clients
        .iter()
        .map(|window| {
            (
                &window.address,
                window.pid,
                &window.class,
                &window.initial_class,
                window.workspace.id,
                &window.workspace.name,
                window.mapped,
            )
        })
        .collect::<Vec<_>>();
    windows.sort_unstable();
    crate::metrics::json_revision(crate::metrics::revision(&(
        target_id, entry, settings, available, windows,
    )))
}

pub(crate) fn page(
    catalog: &Catalog,
    windows: &Snapshot,
    resources: &ResourceSnapshot,
    settings: &SettingsStore,
    params: &QueryParams,
    mut grouped: HashMap<String, Vec<&Client>>,
    ownership_revision: u64,
) -> ApplicationPage {
    let revision = combined_revision(
        catalog,
        windows,
        settings.revision,
        resources.runtime_revision() ^ ownership_revision,
    );
    let available = windows.available;

    for target in resources.target_roots().keys() {
        if catalog.by_id(target).is_none() {
            grouped.entry(target.clone()).or_default();
        }
    }
    let mut applications: Vec<Candidate<'_>> = catalog
        .entries
        .iter()
        .map(|entry| {
            summary_for_entry(
                entry,
                grouped.remove(&entry.id).unwrap_or_default(),
                settings,
                available,
            )
        })
        .collect();
    applications.extend(
        grouped
            .into_iter()
            .map(|(id, clients)| summary_for_unmatched(id, clients, settings, available)),
    );
    let query = params.query.trim().to_lowercase();
    applications.retain_mut(|candidate| {
        (params.category.is_empty() || candidate.identity.category == params.category)
            && rank(candidate, &query)
    });
    applications.sort_by_cached_key(|app| {
        (
            Reverse(app.score),
            app.identity.name.to_lowercase(),
            app.identity.id.clone(),
        )
    });
    let limit = params.limit.clamp(1, 1000);
    let has_more = applications.len() > limit;
    applications.truncate(limit);
    ApplicationPage {
        revision,
        generation: params.generation,
        applications: applications
            .into_iter()
            .map(|candidate| candidate.finish(resources))
            .collect(),
        has_more,
        hyprland_available: available,
    }
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

// Ranking needs metadata and focus order, not process aggregation or window payloads.
struct Candidate<'a> {
    identity: ApplicationIdentity,
    desktop_actions: Vec<crate::model::DesktopActionSummary>,
    revision: u64,
    clients: Vec<&'a Client>,
    focused: bool,
    runtime_score: i64,
    match_score: i64,
    match_kind: &'static str,
    score: i64,
}

impl Candidate<'_> {
    fn finish(self, resources: &ResourceSnapshot) -> ApplicationSummary {
        let usage = resources.usage_for_application(
            &self.identity.id,
            self.clients.iter().map(|window| window.pid),
        );
        let instances = instances(&self.identity.id, &self.clients, resources);
        ApplicationSummary {
            identity: self.identity,
            desktop_actions: self.desktop_actions,
            revision: self.revision,
            runtime: ApplicationRuntime {
                running: !instances.is_empty() || usage.compute.process_count > 0,
                focused: self.focused,
                running_count: instances.len(),
                resources: usage,
                instances,
            },
            runtime_score: self.runtime_score,
            match_score: self.match_score,
            match_kind: self.match_kind.into(),
            score: self.score,
        }
    }
}

fn summary(
    identity: ApplicationIdentity,
    desktop_actions: Vec<crate::model::DesktopActionSummary>,
    clients: Vec<&Client>,
    revision: u64,
) -> Candidate<'_> {
    let focused = clients.iter().any(|window| window.focus_rank == 0);
    let best_rank = clients
        .iter()
        .map(|window| window.focus_rank)
        .min()
        .unwrap_or(i64::MAX);
    let runtime_score = running_score(focused, best_rank);
    Candidate {
        identity,
        revision,
        desktop_actions,
        clients,
        focused,
        match_score: 0,
        match_kind: "none",
        runtime_score,
        score: runtime_score,
    }
}

fn summary_for_entry<'a>(
    entry: &CatalogEntry,
    clients: Vec<&'a Client>,
    settings: &SettingsStore,
    available: bool,
) -> Candidate<'a> {
    let preferences = settings.for_application(&entry.id);
    let revision = target_revision(&entry.id, Some(entry), &clients, preferences, available);
    let identity = ApplicationIdentity {
        id: entry.id.clone(),
        kind: entry.kind().into(),
        name: entry.name.clone(),
        generic_name: entry.generic_name.clone(),
        comment: entry.comment.clone(),
        icon: entry.icon.clone(),
        keywords: entry.keywords.clone(),
        categories: entry.categories.clone(),
        category: preferences
            .map(|value| value.category.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| inferred_category(&entry.categories).into()),
        default_workspace_id: preferences.and_then(|value| value.workspace_id.clone()),
        startup_class: entry.startup_class.clone(),
    };
    summary(identity, entry.actions.clone(), clients, revision)
}

fn summary_for_unmatched<'a>(
    id: String,
    clients: Vec<&'a Client>,
    settings: &SettingsStore,
    available: bool,
) -> Candidate<'a> {
    let revision = target_revision(
        &id,
        None,
        &clients,
        settings.for_application(&id),
        available,
    );
    let name = clients
        .first()
        .filter(|window| !window.class.is_empty())
        .map_or_else(
            || id.strip_prefix("window-group:").unwrap_or(&id),
            |window| &window.class,
        )
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
        category: "shell".into(),
        default_workspace_id: None,
        startup_class: String::new(),
    };
    summary(identity, Vec::new(), clients, revision)
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

fn rank(application: &mut Candidate<'_>, query: &str) -> bool {
    let Some(matched) = search_match(application, query) else {
        return false;
    };
    application.match_score = matched.score;
    application.match_kind = matched.kind;
    application.score = if query.is_empty() {
        application.runtime_score
    } else {
        matched
            .score
            .saturating_mul(100_000)
            .saturating_add(application.runtime_score)
    };
    true
}

// The caller normalizes the query once for the entire page.
fn search_match(application: &Candidate<'_>, query: &str) -> Option<SearchMatch> {
    if query.is_empty() {
        return Some(SearchMatch {
            score: 0,
            kind: "none",
        });
    }

    let name = application.identity.name.to_lowercase();
    let id = application.identity.id.to_lowercase();
    let name_penalty = name.len().min(500) as i64;
    let id_penalty = id.len().min(500) as i64;
    // A contiguous name/ID match already contains every query term. Do not
    // allocate the metadata corpus or acronym for these higher-priority tiers.
    if let Some(matched) = direct_match(&name, &id, id.trim_end_matches(".desktop"), query)
        .or_else(|| substring_match(&name, query, 9_500, 10, name_penalty, "name-substring"))
        .or_else(|| substring_match(&id, query, 9_000, 10, id_penalty, "id-substring"))
    {
        return Some(matched);
    }
    let values = search_values(application);
    let searchable = values.join(" ").to_lowercase();
    let acronym = search_acronym(&values);
    let tokens = query.split_whitespace();
    if !tokens
        .clone()
        .all(|token| searchable.contains(token) || (token.len() <= 5 && acronym.contains(token)))
    {
        return None;
    }
    substring_match(&searchable, query, 7_500, 1, 0, "metadata")
        .or_else(|| acronym_match(query, &acronym))
        .or(Some(SearchMatch {
            score: 5_000 - tokens.count() as i64,
            kind: "terms",
        }))
}

fn acronym_match(query: &str, acronym: &str) -> Option<SearchMatch> {
    let index = (query.len() <= 5).then(|| acronym.find(query)).flatten()?;
    Some(SearchMatch {
        score: 6_500 - index as i64 * 10 - acronym.len().min(500) as i64,
        kind: "acronym",
    })
}

fn substring_match(
    value: &str,
    query: &str,
    base: i64,
    weight: i64,
    length_penalty: i64,
    kind: &'static str,
) -> Option<SearchMatch> {
    value.find(query).map(|index| SearchMatch {
        score: base - index.min(500) as i64 * weight - length_penalty,
        kind,
    })
}

fn direct_match(name: &str, id: &str, id_stem: &str, query: &str) -> Option<SearchMatch> {
    // Ordered tiers: an exact ID beats a name prefix, even for long names.
    [
        (name == query, 12_000, name, "exact-name"),
        (id == query || id_stem == query, 11_800, id, "exact-id"),
        (name.starts_with(query), 11_500, name, "name-prefix"),
        (id.starts_with(query), 11_000, id, "id-prefix"),
    ]
    .into_iter()
    .find_map(|(matches, base, value, kind)| {
        matches.then(|| SearchMatch {
            score: base - value.len().min(500) as i64,
            kind,
        })
    })
}

fn search_values<'a>(application: &'a Candidate<'_>) -> Vec<&'a str> {
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
            .clients
            .iter()
            .flat_map(|window| [window.title.as_str(), window.class.as_str()]),
    )
    .collect()
}

fn search_acronym(values: &[&str]) -> String {
    let mut acronym = String::new();
    for value in values {
        append_initials(&mut acronym, value);
    }
    acronym
}

fn append_initials(acronym: &mut String, value: &str) {
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
