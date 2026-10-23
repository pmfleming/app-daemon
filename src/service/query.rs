use std::{borrow::Cow, cmp::Reverse, collections::HashMap};

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
            Candidate::new(
                Cow::Borrowed(&entry.id),
                Some(entry),
                grouped.remove(&entry.id).unwrap_or_default(),
                settings,
            )
        })
        .collect();
    applications.extend(
        grouped
            .into_iter()
            .map(|(id, clients)| Candidate::new(Cow::Owned(id), None, clients, settings)),
    );
    let query = params.query.trim().to_lowercase();
    applications.retain_mut(|candidate| {
        (params.category.is_empty() || candidate.category() == params.category)
            && rank(candidate, &query)
    });
    applications.sort_by_cached_key(|app| {
        (
            Reverse(app.score),
            app.name().to_lowercase(),
            app.id.clone(),
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
            .map(|candidate| candidate.finish(resources, available))
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

// Own only unmatched IDs and window grouping. Catalog metadata, preferences and
// revision inputs remain borrowed until a row survives filtering and pagination.
struct Candidate<'a> {
    id: Cow<'a, str>,
    entry: Option<&'a CatalogEntry>,
    preferences: Option<&'a ApplicationSettings>,
    clients: Vec<&'a Client>,
    focused: bool,
    runtime_score: i64,
    match_score: i64,
    match_kind: &'static str,
    score: i64,
}

impl<'a> Candidate<'a> {
    fn new(
        id: Cow<'a, str>,
        entry: Option<&'a CatalogEntry>,
        clients: Vec<&'a Client>,
        settings: &'a SettingsStore,
    ) -> Self {
        let focused = clients.iter().any(|window| window.focus_rank == 0);
        let best_rank = clients
            .iter()
            .map(|window| window.focus_rank)
            .min()
            .unwrap_or(i64::MAX);
        let runtime_score = running_score(focused, best_rank);
        Self {
            preferences: settings.for_application(&id),
            id,
            entry,
            clients,
            focused,
            match_score: 0,
            match_kind: "none",
            runtime_score,
            score: runtime_score,
        }
    }

    fn name(&self) -> &str {
        self.entry.map_or_else(
            || {
                self.clients
                    .first()
                    .filter(|window| !window.class.is_empty())
                    .map_or_else(
                        || self.id.strip_prefix("window-group:").unwrap_or(&self.id),
                        |window| window.class.as_str(),
                    )
            },
            |entry| entry.name.as_str(),
        )
    }

    fn category(&self) -> &str {
        self.entry.map_or("shell", |entry| {
            self.preferences
                .map(|value| value.category.as_str())
                .filter(|category| !category.is_empty())
                .unwrap_or_else(|| inferred_category(&entry.categories))
        })
    }

    fn fields(&self) -> [&str; 5] {
        [
            self.name(),
            self.entry
                .map_or("Running window", |entry| entry.generic_name.as_str()),
            self.entry.map_or("", |entry| entry.comment.as_str()),
            &self.id,
            self.entry.map_or("", |entry| entry.startup_class.as_str()),
        ]
    }

    fn keywords(&self) -> impl Iterator<Item = &str> {
        self.entry
            .into_iter()
            .flat_map(|entry| entry.keywords.iter().map(String::as_str))
            .chain(window_fields(&self.clients).filter(|_| self.entry.is_none()))
    }

    fn into_identity(self) -> ApplicationIdentity {
        let [name, generic_name, comment, _, startup_class] = self.fields();
        ApplicationIdentity {
            kind: self.entry.map_or("window-group", CatalogEntry::kind).into(),
            name: name.into(),
            generic_name: generic_name.into(),
            comment: comment.into(),
            startup_class: startup_class.into(),
            icon: self
                .entry
                .map(|entry| entry.icon.clone())
                .unwrap_or_default(),
            keywords: self.keywords().map(str::to_owned).collect(),
            categories: self
                .entry
                .map(|entry| entry.categories.clone())
                .unwrap_or_default(),
            category: self.category().into(),
            default_workspace_id: self
                .entry
                .and(self.preferences)
                .and_then(|value| value.workspace_id.clone()),
            id: self.id.into_owned(),
        }
    }

    fn finish(self, resources: &ResourceSnapshot, available: bool) -> ApplicationSummary {
        let usage =
            resources.usage_for_application(&self.id, self.clients.iter().map(|window| window.pid));
        let instances = instances(&self.id, &self.clients, resources);
        ApplicationSummary {
            desktop_actions: self
                .entry
                .map(|entry| entry.actions.clone())
                .unwrap_or_default(),
            revision: target_revision(
                &self.id,
                self.entry,
                &self.clients,
                self.preferences,
                available,
            ),
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
            identity: self.into_identity(),
        }
    }
}

fn window_fields<'a>(clients: &'a [&Client]) -> impl Iterator<Item = &'a str> {
    clients
        .iter()
        .flat_map(|window| [window.title.as_str(), window.class.as_str()])
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

    let name = application.name().to_lowercase();
    let id = application.id.to_lowercase();
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
    let (searchable, acronym) = search_corpus(application);
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

fn search_corpus(application: &Candidate<'_>) -> (String, String) {
    let values = || {
        application
            .fields()
            .into_iter()
            .chain(application.keywords())
            .chain(
                application
                    .entry
                    .into_iter()
                    .flat_map(|entry| entry.categories.iter().map(String::as_str)),
            )
            .chain(window_fields(&application.clients))
    };
    let mut searchable = String::with_capacity(values().map(|value| value.len() + 1).sum());
    let mut acronym = String::new();
    for value in values() {
        // A trailing separator cannot match the caller's trimmed query; keeping
        // it avoids a separate first-field branch and preserves match offsets.
        searchable.push_str(value);
        searchable.push(' ');
        append_initials(&mut acronym, value);
    }
    (searchable.to_lowercase(), acronym)
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
