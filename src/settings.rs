use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone)]
pub struct SettingsStore {
    applications: BTreeMap<String, ApplicationSettings>,
    pub revision: u64,
}

impl SettingsStore {
    pub(crate) fn from_entries(applications: BTreeMap<String, ApplicationSettings>) -> Self {
        Self {
            revision: crate::metrics::revision(&applications),
            applications,
        }
    }
    pub(crate) fn entries(&self) -> &BTreeMap<String, ApplicationSettings> {
        &self.applications
    }
    pub(crate) fn category_for<'a>(&'a self, id: &str, categories: &[String]) -> &'a str {
        self.for_application(id)
            .filter(|s| s.category_set)
            .map(|s| s.category.as_str())
            .unwrap_or_else(|| inferred_category(categories))
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
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::from_entries(BTreeMap::new())
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
    fn omitted_category_is_distinct_from_explicit_null() {
        let update: crate::service::UpdateSettingsParams =
            serde_json::from_str(r#"{"target_id":"a","new_workspace":true}"#).unwrap();
        assert_eq!(update.category, None);
        let clear: crate::service::UpdateSettingsParams =
            serde_json::from_str(r#"{"target_id":"a","category":null}"#).unwrap();
        assert_eq!(clear.category, Some(None));
        let (store, settings) = SettingsStore::default()
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
