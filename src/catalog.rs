use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use freedesktop_desktop_entry::{DesktopEntry, default_paths, get_languages_from_env};
use walkdir::WalkDir;

use crate::model::DesktopActionSummary;

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
    entry: DesktopEntry,
}

impl CatalogEntry {
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

impl Catalog {
    pub fn load() -> Self {
        Self::from_paths(default_paths().collect())
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

fn catalog_entry(
    path: PathBuf,
    id: String,
    locales: &[String],
    desktops: &[String],
) -> Option<CatalogEntry> {
    let entry = DesktopEntry::from_path(path, Some(locales)).ok()?;
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
    entry.type_() == Some("Application")
        && !entry.hidden()
        && !entry.no_display()
        && entry.exec().is_some_and(|value| !value.is_empty())
        && entry.try_exec().is_none_or(executable_available)
        && entry
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

fn executable_available(value: &str) -> bool {
    let executable = Path::new(value);
    if executable.components().count() > 1 {
        return is_executable(executable);
    }
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| is_executable(&dir.join(value))))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
        entry.id.hash(&mut hasher);
        entry.name.hash(&mut hasher);
        entry.icon.hash(&mut hasher);
        entry.startup_class.hash(&mut hasher);
        entry
            .actions
            .iter()
            .for_each(|action| action.id.hash(&mut hasher));
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Catalog;

    #[test]
    fn preserves_empty_optional_fields_and_honors_precedence() -> anyhow::Result<()> {
        let high = tempfile::tempdir()?;
        let low = tempfile::tempdir()?;
        fs::write(
            high.path().join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nHidden=true\n",
        )?;
        fs::write(
            low.path().join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Visible lower copy\nExec=true\n",
        )?;
        fs::write(
            high.path().join("plain.desktop"),
            "[Desktop Entry]\nType=Application\nName=Plain\nExec=true\n",
        )?;

        let catalog = Catalog::from_paths(vec![high.path().into(), low.path().into()]);
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].id, "plain.desktop");
        assert_eq!(catalog.entries[0].icon, "");
        assert_eq!(catalog.entries[0].startup_class, "");
        Ok(())
    }
}
