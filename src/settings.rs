use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const CATEGORIES: &[&str] = &["shell", "browser", "code", "media", "text"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplicationSettings {
    pub category: String,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SettingsFile {
    version: u8,
    applications: BTreeMap<String, ApplicationSettings>,
}

#[derive(Debug)]
pub struct SettingsStore {
    path: Option<PathBuf>,
    applications: BTreeMap<String, ApplicationSettings>,
    pub revision: u64,
}

impl SettingsStore {
    pub fn load_default() -> Self {
        Self::load(settings_path())
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let applications = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<SettingsFile>(&bytes).ok())
            .filter(|file| file.version == 1)
            .map(|file| file.applications)
            .unwrap_or_default();
        let revision = settings_revision(&applications);
        Self {
            path,
            applications,
            revision,
        }
    }

    pub fn for_application(&self, target_id: &str) -> Option<&ApplicationSettings> {
        self.applications.get(target_id)
    }

    pub fn update(
        &mut self,
        target_id: String,
        category: String,
        workspace_id: Option<String>,
    ) -> anyhow::Result<ApplicationSettings> {
        anyhow::ensure!(
            CATEGORIES.contains(&category.as_str()),
            "application category is invalid"
        );
        if let Some(workspace) = workspace_id.as_deref() {
            anyhow::ensure!(
                !workspace.is_empty() && workspace.chars().all(|value| value.is_ascii_digit()),
                "default workspace is invalid"
            );
        }
        let settings = ApplicationSettings {
            category,
            workspace_id,
        };
        let previous = self
            .applications
            .insert(target_id.clone(), settings.clone());
        self.revision = settings_revision(&self.applications);
        if let Err(error) = self.persist() {
            if let Some(previous) = previous {
                self.applications.insert(target_id, previous);
            } else {
                self.applications.remove(&target_id);
            }
            self.revision = settings_revision(&self.applications);
            return Err(error.into());
        }
        Ok(settings)
    }

    fn persist(&self) -> std::io::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&SettingsFile {
            version: 1,
            applications: self.applications.clone(),
        })?;
        let temporary = temporary_path(path);
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
    }
}

pub fn inferred_category(categories: &[String]) -> &'static str {
    let has = |candidate: &str| {
        categories
            .iter()
            .any(|value| value.eq_ignore_ascii_case(candidate))
    };
    if has("WebBrowser") || has("Network") {
        "browser"
    } else if has("Development") {
        "code"
    } else if has("AudioVideo") || has("Audio") || has("Video") || has("Graphics") {
        "media"
    } else if has("Office") || has("TextEditor") || has("WordProcessor") || has("Viewer") {
        "text"
    } else {
        "shell"
    }
}

fn settings_revision(applications: &BTreeMap<String, ApplicationSettings>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for (id, settings) in applications {
        id.hash(&mut hasher);
        settings.category.hash(&mut hasher);
        settings.workspace_id.hash(&mut hasher);
    }
    hasher.finish()
}

fn settings_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("app-daemon/application-settings-v1.json"))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::{SettingsStore, inferred_category};

    #[test]
    fn infers_the_five_launcher_categories() {
        assert_eq!(inferred_category(&["TerminalEmulator".into()]), "shell");
        assert_eq!(inferred_category(&["WebBrowser".into()]), "browser");
        assert_eq!(inferred_category(&["Development".into()]), "code");
        assert_eq!(inferred_category(&["AudioVideo".into()]), "media");
        assert_eq!(inferred_category(&["TextEditor".into()]), "text");
    }

    #[test]
    fn persists_application_preferences() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("settings.json");
        let mut store = SettingsStore::load(Some(path.clone()));
        store.update("example.desktop".into(), "code".into(), Some("3".into()))?;
        let loaded = SettingsStore::load(Some(path));
        let settings = loaded
            .for_application("example.desktop")
            .expect("saved settings");
        assert_eq!(settings.category, "code");
        assert_eq!(settings.workspace_id.as_deref(), Some("3"));
        Ok(())
    }
}
