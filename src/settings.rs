use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{
    AtomicWritePolicy, XdgRoot, read_json, resolve_xdg_path, write_json_atomic,
};

const CATEGORY_WORKSPACES: &[(&str, &str)] = &[
    ("shell", "1"),
    ("browser", "2"),
    ("code", "3"),
    ("media", "4"),
    ("text", "5"),
];
const CATEGORY_HINTS: &[(&str, &[&str])] = &[
    ("browser", &["WebBrowser", "Network"]),
    ("code", &["Development"]),
    ("media", &["AudioVideo", "Audio", "Video", "Graphics"]),
    ("text", &["Office", "TextEditor", "WordProcessor", "Viewer"]),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplicationSettings {
    pub category: String,
    pub workspace_id: Option<String>,
    pub new_workspace: bool,
    pub category_set: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SettingsFile<T = BTreeMap<String, ApplicationSettings>> {
    version: u8,
    applications: T,
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: Option<PathBuf>,
    applications: BTreeMap<String, ApplicationSettings>,
    pub revision: u64,
}

impl SettingsStore {
    pub fn load_default() -> Self {
        Self::load(resolve_xdg_path(
            XdgRoot::Config,
            "app-daemon",
            Path::new("application-settings-v2.json"),
        ))
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let file = path
            .as_ref()
            .and_then(|path| read_json::<SettingsFile>(path).ok().flatten())
            .filter(|file| file.version == 2 || file.version == 1);
        // Import v1 only when v2 is absent. Never overwrite the downgrade copy.
        let file = file.or_else(|| {
            let path = path.as_ref()?;
            if path.exists() {
                return None;
            }
            read_json::<SettingsFile>(&path.with_file_name("application-settings-v1.json"))
                .ok()
                .flatten()
                .filter(|file| file.version == 1)
        });
        let migrated = file.as_ref().is_some_and(|file| file.version == 1);
        let mut applications = file.map(|file| file.applications).unwrap_or_default();
        applications.retain(|_, settings| {
            if migrated {
                settings.category_set = true;
            }
            settings.workspace_id = workspace_for_category(&settings.category).map(str::to_owned);
            settings.category.is_empty() || settings.workspace_id.is_some()
        });
        let revision = crate::metrics::revision(&applications);
        Self {
            path,
            applications,
            revision,
        }
    }

    pub fn for_application(&self, target_id: &str) -> Option<&ApplicationSettings> {
        self.applications.get(target_id)
    }

    #[cfg(test)]
    pub fn prepare_update(
        &self,
        target_id: String,
        category: String,
    ) -> anyhow::Result<(Self, ApplicationSettings)> {
        self.prepare_patch(target_id, Some(Some(category)), None)
    }

    pub fn prepare_patch(
        &self,
        target_id: String,
        category: Option<Option<String>>,
        new_workspace: Option<bool>,
    ) -> anyhow::Result<(Self, ApplicationSettings)> {
        let mut settings = self
            .applications
            .get(&target_id)
            .cloned()
            .unwrap_or_default();
        if let Some(category) = category {
            settings.category_set = true;
            settings.category = category.unwrap_or_default();
            anyhow::ensure!(
                settings.category.is_empty()
                    || workspace_for_category(&settings.category).is_some(),
                "application category is invalid"
            );
            settings.workspace_id = workspace_for_category(&settings.category).map(str::to_owned);
        }
        if let Some(value) = new_workspace {
            settings.new_workspace = value;
        }
        let mut next = self.clone();
        next.applications.insert(target_id, settings.clone());
        next.revision = crate::metrics::revision(&next.applications);
        Ok((next, settings))
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        write_json_atomic(
            path,
            &SettingsFile {
                version: 2,
                applications: &self.applications,
            },
            AtomicWritePolicy {
                pretty: false,
                ..AtomicWritePolicy::PRIVATE
            },
        )
        .map_err(std::io::Error::other)
    }
}

pub fn workspace_for_category(category: &str) -> Option<&'static str> {
    CATEGORY_WORKSPACES
        .iter()
        .find_map(|(candidate, workspace)| (*candidate == category).then_some(*workspace))
}

pub fn inferred_category(categories: &[String]) -> &'static str {
    CATEGORY_HINTS
        .iter()
        .find(|(_, hints)| {
            hints.iter().any(|hint| {
                categories
                    .iter()
                    .any(|category| category.eq_ignore_ascii_case(hint))
            })
        })
        .map_or("shell", |(category, _)| category)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_v1_and_independent_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("application-settings-v1.json");
        let original =
            r#"{"version":1,"applications":{"editor":{"category":"code","workspace_id":"999"}}}"#;
        std::fs::write(&old, original).unwrap();
        let path = directory.path().join("application-settings-v2.json");
        let store = SettingsStore::load(Some(path.clone()));
        let imported = store.for_application("editor").unwrap();
        assert_eq!(imported.workspace_id.as_deref(), Some("3"));
        assert!(!imported.new_workspace);
        let (store, settings) = store
            .prepare_patch("editor".into(), None, Some(true))
            .unwrap();
        assert_eq!(settings.category, "code");
        assert!(settings.new_workspace);
        let (store, settings) = store
            .prepare_patch("editor".into(), Some(None), None)
            .unwrap();
        assert_eq!(settings.workspace_id, None);
        assert!(settings.new_workspace && settings.category_set);
        store.persist().unwrap();
        assert_eq!(std::fs::read_to_string(old).unwrap(), original);
        assert_eq!(
            SettingsStore::load(Some(path)).for_application("editor"),
            Some(&settings)
        );
    }

    #[test]
    fn omitted_category_is_distinct_from_explicit_null() {
        let update: crate::service::UpdateSettingsParams =
            serde_json::from_str(r#"{"target_id":"a","new_workspace":true}"#).unwrap();
        assert_eq!(update.category, None);
        let clear: crate::service::UpdateSettingsParams =
            serde_json::from_str(r#"{"target_id":"a","category":null}"#).unwrap();
        assert_eq!(clear.category, Some(None));
        let (store, settings) = SettingsStore::load(None)
            .prepare_patch("a".into(), update.category, update.new_workspace)
            .unwrap();
        assert!(!settings.category_set);
        assert!(
            store
                .prepare_patch("a".into(), Some(Some("invalid".into())), None)
                .is_err()
        );
    }
}
