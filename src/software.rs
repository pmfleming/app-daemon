//! Shared, user-owned software catalog. Worker observations are evidence, never
//! launch instructions. Desktop identities remain the launcher's exact IDs.
use crate::{catalog::Catalog, settings::SettingsStore};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};
pub(crate) mod store;
#[cfg(test)]
mod tests;

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogParams {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Release {
    pub version: Option<String>,
    pub source: String,
    pub checked_at: Option<u64>,
    pub attempted_at: Option<u64>,
    pub stale: bool,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Installation {
    pub stale: bool,
    pub scope: String,
    pub package_name: String,
    pub store_path: Option<String>,
    pub declared_version: Option<String>,
    pub active_version: Option<String>,
    pub version_origin: String,
    pub release: Option<Release>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub kind: String,
    pub category: String,
    pub installations: Vec<Installation>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Snapshot {
    pub revision: u64,
    pub observed_at: u64,
    pub inventory_available: bool,
    pub inventory_total: usize,
    pub inventory_included: usize,
    pub error: Option<String>,
    pub entries: Vec<Record>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Package {
    pub scope: String,
    pub name: String,
    pub version: Option<String>,
    pub release: Option<Release>,
    pub store_path: Option<String>,
    #[serde(default)]
    pub desktop_ids: Vec<String>,
    pub kind: Option<String>,
    pub vendor_tool: Option<String>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Inventory {
    pub available: bool,
    pub total: usize,
    pub packages: Vec<Package>,
    pub error: Option<String>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Observations {
    pub schema_version: u32,
    pub applications: Inventory,
    pub tools: Vec<Value>,
    #[serde(default)]
    pub stale: bool,
}
impl Observations {
    pub(crate) fn mark_unavailable(&mut self, error: String) {
        self.stale = true;
        self.applications.available = false;
        self.applications.error = Some(error);
    }
    #[cfg(test)]
    pub(crate) fn unavailable(error: String) -> Self {
        Self {
            applications: Inventory {
                error: Some(error),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        ensure!(
            value.schema_version == 1
                && value.applications.packages.len() <= 512
                && value.applications.total <= 4096
                && value.tools.len() <= 5,
            "invalid worker catalog bounds"
        );
        ensure!(
            value
                .applications
                .packages
                .iter()
                .all(|p| !p.name.is_empty()
                    && p.name.len() <= 160
                    && p.scope.len() <= 80
                    && p.desktop_ids.len() <= 32
                    && p.desktop_ids.iter().all(|id| id.len() <= 160)),
            "invalid worker package identity"
        );
        Ok(value)
    }
}

pub(crate) async fn read_worker(path: &Path) -> Result<Observations> {
    use tokio::io::AsyncReadExt;
    ensure!(path.is_absolute(), "worker path must be absolute");
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut child = tokio::process::Command::new(path)
            .args(["application-status", "--json"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("missing catalog output")?
            .take(2_097_153)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 2_097_152, "worker catalog exceeds 2 MiB");
        ensure!(
            child.wait().await?.success(),
            "worker application-status unavailable; install matching update-worker"
        );
        Observations::decode(&bytes)
    })
    .await
    .context("worker catalog timed out")?
}

pub(crate) struct Cache {
    pub store: store::Store,
    pub observations: Observations,
    pub attempted: Option<Instant>,
    pub worker: std::path::PathBuf,
}

impl Package {
    fn installation(&self, observations: &Observations) -> Installation {
        let tool = self
            .vendor_tool
            .as_deref()
            .and_then(|id| observations.tools.iter().find(|t| t["tool"] == id));
        let active = tool
            .and_then(|t| t["installed_version"].as_str())
            .map(str::to_owned);
        let release = tool
            .and_then(|t| {
                let source = t["latest_release"]["source"].as_str()?;
                let version = t["latest_version"].as_str()?;
                let checked_at = t["last_successful_check"].as_u64()?;
                Some(Release {
                    version: Some(version.into()),
                    source: source.into(),
                    checked_at: Some(checked_at),
                    attempted_at: t["last_attempt"].as_u64().or(Some(checked_at)),
                    stale: t["stale"].as_bool().unwrap_or(true),
                    // Installation/build failures do not invalidate successful release observations.
                    error: None,
                })
            })
            .or_else(|| self.release.clone())
            .map(|mut release| {
                release.stale |= observations.stale;
                release
            });
        Installation {
            stale: observations.stale,
            scope: self.scope.clone(),
            package_name: self.name.clone(),
            store_path: self.store_path.clone(),
            declared_version: self.version.clone(),
            version_origin: if active.is_some() {
                "active-profile"
            } else {
                "generation"
            }
            .into(),
            active_version: active,
            release,
        }
    }
}

pub(crate) fn compose(
    catalog: &Catalog,
    settings: &SettingsStore,
    observations: &Observations,
) -> Snapshot {
    let owners: BTreeMap<_, _> = catalog
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry.store_path()))
        .collect();
    let mut records: BTreeMap<String, Record> = catalog
        .entries
        .iter()
        .map(|entry| {
            (
                entry.id.clone(),
                Record {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    icon: entry.icon.clone(),
                    kind: entry.kind().into(),
                    category: settings.category_for(&entry.id, &entry.categories).into(),
                    installations: Vec::new(),
                },
            )
        })
        .collect();
    for package in &observations.applications.packages {
        let installation = package.installation(observations);
        // An explicit reviewed desktop ID wins. Otherwise match the exact store
        // output owning the desktop file. Never infer identity from display names.
        let mut matches = records.iter_mut().filter(|(id, _)| {
            owners.get(id.as_str()).is_some_and(|path| {
                package.desktop_ids.contains(*id)
                    || package
                        .store_path
                        .as_ref()
                        .is_some_and(|value| path.as_ref() == Some(value))
            })
        });
        if let Some((_, first)) = matches.next() {
            // Only additional desktop identities need a copy of the installation.
            for (_, record) in matches {
                record.installations.push(installation.clone());
            }
            first.installations.push(installation);
        } else {
            let id = format!("package:{}", package.name);
            records
                .entry(id.clone())
                .or_insert_with(|| Record {
                    id,
                    name: package.name.clone(),
                    icon: "application-x-executable".into(),
                    kind: package.kind.clone().unwrap_or_else(|| "system".into()),
                    category: String::new(),
                    installations: Vec::new(),
                })
                .installations
                .push(installation);
        }
    }
    let mut entries: Vec<_> = records.into_values().collect();
    for record in &mut entries {
        record.installations.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then_with(|| a.package_name.cmp(&b.package_name))
                .then_with(|| a.store_path.cmp(&b.store_path))
                .then_with(|| a.declared_version.cmp(&b.declared_version))
        });
        record.installations.dedup();
    }
    entries.sort_by_cached_key(|r| {
        (
            group_rank(&r.kind),
            r.category.clone(),
            r.name.to_lowercase(),
            r.id.clone(),
        )
    });
    let revision = crate::metrics::json_revision(crate::metrics::revision(
        &serde_json::to_string(&(
            &entries,
            observations.applications.available,
            &observations.applications.error,
            observations.applications.total,
        ))
        .unwrap(),
    ));
    Snapshot {
        revision,
        observed_at: crate::history::now_milliseconds() / 1000,
        inventory_available: observations.applications.available,
        inventory_total: observations.applications.total,
        inventory_included: observations.applications.packages.len(),
        error: observations.applications.error.clone(),
        entries,
    }
}
fn group_rank(kind: &str) -> u8 {
    match kind {
        "desktop-application" => 0,
        "desktop-shortcut" => 1,
        "command-line" => 2,
        "system" => 3,
        _ => 4,
    }
}
