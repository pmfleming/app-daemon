use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    env,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use freedesktop_desktop_entry::{DesktopEntry, default_paths, get_languages_from_env};
use walkdir::WalkDir;

use crate::{model::DesktopActionSummary, platform::command_available};

#[derive(Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub icon: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub startup_class: String,
    pub actions: Vec<DesktopActionSummary>,
    pub launch_only: bool,
    entry: DesktopEntry,
}

impl CatalogEntry {
    pub fn launch_command(&self) -> anyhow::Result<Vec<String>> {
        self.entry.parse_exec().map_err(anyhow::Error::from)
    }

    pub fn requires_terminal(&self) -> bool {
        self.entry.terminal()
    }

    pub fn kind(&self) -> &'static str {
        if self.launch_only {
            "desktop-shortcut"
        } else {
            "desktop-application"
        }
    }

    pub fn parse_action(&self, action_id: &str) -> anyhow::Result<Vec<String>> {
        anyhow::ensure!(
            self.actions.iter().any(|action| action.id == action_id),
            "desktop action is unavailable"
        );
        self.entry
            .parse_exec_action(action_id)
            .map_err(anyhow::Error::from)
    }
}

#[derive(Debug)]
pub struct Catalog {
    pub revision: u64,
    pub entries: Vec<CatalogEntry>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            revision: 0,
            entries: Vec::new(),
        }
    }
}

impl Catalog {
    pub fn load() -> Self {
        Self::from_paths(default_catalog_paths())
    }

    pub fn from_paths(paths: Vec<PathBuf>) -> Self {
        let locales = get_languages_from_env();
        let desktops = current_desktops();
        let mut seen_ids = HashSet::new();
        let mut entries = paths
            .iter()
            .flat_map(|root| {
                desktop_files(root)
                    .into_iter()
                    .map(move |path| (root, path))
            })
            .filter_map(|(root, path)| {
                let id = desktop_id(root, &path)?;
                if !seen_ids.insert(id.clone()) {
                    return None;
                }
                catalog_entry(path, id, &locales, &desktops)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        let revision = catalog_revision(&entries);
        Self { revision, entries }
    }

    pub fn by_id(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }
}

pub fn default_catalog_paths() -> Vec<PathBuf> {
    default_paths().collect()
}

fn catalog_entry(
    path: PathBuf,
    id: String,
    locales: &[String],
    desktops: &[String],
) -> Option<CatalogEntry> {
    let entry = DesktopEntry::from_path(&path, Some(locales)).ok()?;
    let name = entry.name(locales).filter(|value| !value.is_empty())?;
    if !visible(&entry, desktops) {
        return None;
    }
    Some(CatalogEntry {
        id,
        name: name.into_owned(),
        generic_name: entry.generic_name(locales).unwrap_or_default().into_owned(),
        comment: entry.comment(locales).unwrap_or_default().into_owned(),
        icon: entry.icon().unwrap_or_default().to_owned(),
        keywords: strings(entry.keywords(locales).unwrap_or_default()),
        categories: entry
            .categories()
            .unwrap_or_default()
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        startup_class: entry.startup_wm_class().unwrap_or_default().to_owned(),
        actions: desktop_actions(&entry, locales),
        launch_only: entry.desktop_entry("X-Shelllist-LaunchOnly") == Some("true"),
        entry,
    })
}

fn desktop_actions(entry: &DesktopEntry, locales: &[String]) -> Vec<DesktopActionSummary> {
    entry
        .actions()
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !id.is_empty())
        .filter_map(|id| {
            Some(DesktopActionSummary {
                id: id.to_owned(),
                name: entry.action_name(id, locales)?.into_owned(),
                icon: entry
                    .action_entry(id, "Icon")
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

fn strings(values: Vec<std::borrow::Cow<'_, str>>) -> Vec<String> {
    values
        .into_iter()
        .filter(|value| !value.is_empty())
        .map(|value| value.into_owned())
        .collect()
}

fn current_desktops() -> Vec<String> {
    env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn visible(entry: &DesktopEntry, desktops: &[String]) -> bool {
    launchable(entry) && shown_on_desktop(entry, desktops)
}

fn launchable(entry: &DesktopEntry) -> bool {
    entry.type_() == Some("Application")
        && !entry.hidden()
        && !entry.no_display()
        && entry.exec().is_some_and(|value| !value.is_empty())
        && entry.try_exec().is_none_or(command_available)
}

fn shown_on_desktop(entry: &DesktopEntry, desktops: &[String]) -> bool {
    entry
        .only_show_in()
        .is_none_or(|only| list_matches(only, desktops))
        && entry
            .not_show_in()
            .is_none_or(|excluded| !list_matches(excluded, desktops))
}

fn list_matches(values: Vec<&str>, desktops: &[String]) -> bool {
    values.iter().any(|value| {
        !value.is_empty()
            && desktops
                .iter()
                .any(|desktop| value.eq_ignore_ascii_case(desktop))
    })
}

fn desktop_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "desktop")
        })
        .collect()
}

fn desktop_id(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(relative.to_string_lossy().replace('/', "-"))
}

fn catalog_revision(entries: &[CatalogEntry]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for entry in entries {
        (
            &entry.id,
            &entry.name,
            &entry.generic_name,
            &entry.comment,
            &entry.icon,
            &entry.keywords,
            &entry.categories,
            &entry.startup_class,
            &entry.actions,
            entry.launch_only,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests;
