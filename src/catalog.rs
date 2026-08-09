use std::{
    collections::{BTreeSet, HashSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use freedesktop_desktop_entry::{DesktopEntry, default_paths, get_languages_from_env};

use crate::model::DesktopActionSummary;

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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
        let mut entries = Vec::new();

        for root in paths {
            for path in desktop_files(&root) {
                let Some(id) = desktop_id(&root, &path) else {
                    continue;
                };
                if !seen_ids.insert(id.clone()) {
                    continue;
                }
                let Ok(entry) = DesktopEntry::from_path(path, Some(&locales)) else {
                    continue;
                };
                if !visible(&entry, &desktops) {
                    continue;
                }
                let Some(name) = entry.name(&locales).filter(|value| !value.is_empty()) else {
                    continue;
                };
                let actions = entry
                    .actions()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|id| !id.is_empty())
                    .filter_map(|action_id| {
                        let name = entry.action_name(action_id, &locales)?.to_string();
                        Some(DesktopActionSummary {
                            id: action_id.to_owned(),
                            name,
                            icon: entry
                                .action_entry(action_id, "Icon")
                                .unwrap_or("")
                                .to_owned(),
                        })
                    })
                    .collect();
                entries.push(CatalogEntry {
                    id,
                    name: name.to_string(),
                    generic_name: entry.generic_name(&locales).unwrap_or_default().to_string(),
                    comment: entry.comment(&locales).unwrap_or_default().to_string(),
                    icon: entry.icon().unwrap_or("").to_owned(),
                    keywords: strings(entry.keywords(&locales).unwrap_or_default()),
                    categories: entry
                        .categories()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    startup_class: entry.startup_wm_class().unwrap_or("").to_owned(),
                    actions,
                    entry,
                });
            }
        }
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
    if entry.type_() != Some("Application")
        || entry.hidden()
        || entry.no_display()
        || entry.exec().is_none_or(str::is_empty)
        || entry
            .try_exec()
            .is_some_and(|value| !executable_available(value))
    {
        return false;
    }
    if let Some(only) = entry.only_show_in()
        && !only.iter().filter(|value| !value.is_empty()).any(|value| {
            desktops
                .iter()
                .any(|desktop| value.eq_ignore_ascii_case(desktop))
        })
    {
        return false;
    }
    if let Some(excluded) = entry.not_show_in()
        && excluded.iter().any(|value| {
            desktops
                .iter()
                .any(|desktop| value.eq_ignore_ascii_case(desktop))
        })
    {
        return false;
    }
    true
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
    fn visit(path: &Path, files: &mut Vec<PathBuf>, visited: &mut BTreeSet<PathBuf>) {
        let Ok(canonical) = path.canonicalize() else {
            return;
        };
        if !visited.insert(canonical) {
            return;
        }
        let Ok(read_dir) = fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<_> = read_dir
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            if child.is_dir() {
                visit(&child, files, visited);
            } else if child
                .extension()
                .is_some_and(|extension| extension == "desktop")
            {
                files.push(child);
            }
        }
    }
    let mut files = Vec::new();
    visit(root, &mut files, &mut BTreeSet::new());
    files
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
    fn preserves_empty_optional_fields_and_honors_precedence() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        fs::write(
            high.path().join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nHidden=true\n",
        )
        .unwrap();
        fs::write(
            low.path().join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Visible lower copy\nExec=true\n",
        )
        .unwrap();
        fs::write(
            high.path().join("plain.desktop"),
            "[Desktop Entry]\nType=Application\nName=Plain\nExec=true\n",
        )
        .unwrap();

        let catalog = Catalog::from_paths(vec![high.path().into(), low.path().into()]);
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].id, "plain.desktop");
        assert_eq!(catalog.entries[0].icon, "");
        assert_eq!(catalog.entries[0].startup_class, "");
    }
}
