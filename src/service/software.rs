use super::*;
use crate::software::{self, Snapshot};

impl ApplicationService {
    pub async fn software_catalog(&self, refresh: bool) -> anyhow::Result<Snapshot> {
        Ok(self.software_view(refresh).await?.0)
    }

    pub(super) async fn software_view(
        &self,
        refresh: bool,
    ) -> anyhow::Result<(Snapshot, Arc<Catalog>, Arc<SettingsStore>)> {
        let _settings_update = self.settings_updates.lock().await;
        let catalog = Arc::clone(&*self.catalog.read().await);
        let settings = Arc::clone(&*self.settings.read().await);
        let mut held = Arc::clone(&self.software).lock_owned().await;
        let Some(cache) = held.as_mut() else {
            // Only synthetic services omit persistent storage.
            let snapshot = software::compose(&catalog, &settings, &Default::default());
            return Ok((snapshot, catalog, settings));
        };
        if refresh
            || cache
                .attempted
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            match software::read_worker(&cache.worker).await {
                Ok(observations) => cache.observations = observations,
                Err(error) => cache.observations.mark_unavailable(format!("{error:#}")),
            }
            cache.attempted = Some(Instant::now());
        }
        let (snapshot, catalog, settings) = tokio::task::spawn_blocking(move || {
            let cache = held
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("software cache missing"))?;
            let next = software::compose(&catalog, &settings, &cache.observations);
            let snapshot = match cache.store.snapshot()? {
                Some(current) if current.revision == next.revision => current,
                _ => {
                    cache.store.replace(&next, None, &cache.observations)?;
                    next
                }
            };
            Ok::<_, anyhow::Error>((snapshot, catalog, settings))
        })
        .await??;
        let previous = self
            .software_revision
            .swap(snapshot.revision, Ordering::AcqRel);
        if previous != snapshot.revision {
            self.publish_state().await;
        }
        Ok((snapshot, catalog, settings))
    }

    pub(super) async fn persist_software_settings(
        &self,
        settings: Arc<SettingsStore>,
        update: tokio::sync::OwnedMutexGuard<()>,
    ) -> anyhow::Result<()> {
        let mut held = Arc::clone(&self.software).lock_owned().await;
        let catalog = Arc::clone(&*self.catalog.read().await);
        let memory = Arc::clone(&self.settings);
        let revision = Arc::clone(&self.software_revision);
        tokio::task::spawn_blocking(move || {
            let _update = update;
            if let Some(cache) = held.as_mut() {
                let snapshot = software::compose(&catalog, &settings, &cache.observations);
                cache
                    .store
                    .replace(&snapshot, Some(&settings), &cache.observations)?;
                revision.store(snapshot.revision, Ordering::Release);
            }
            // Publish the acknowledged preference in the same non-cancellable
            // task as its commit, before admitting the next category change.
            *memory.blocking_write() = settings;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}

pub(super) async fn track(service: std::sync::Weak<ApplicationService>) {
    let mut interval = time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(service) = service.upgrade() else {
            return;
        };
        if let Err(error) = service.software_catalog(false).await {
            tracing::warn!(%error, "software catalog refresh failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn launcher_and_catalog_share_durable_category_and_version_records() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("editor.desktop"), "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor\nCategories=Development;\n").unwrap();
        let service =
            ApplicationService::with_fixture_catalog(crate::catalog::Catalog::from_paths(vec![
                directory.path().into(),
            ]))
            .await;
        let observations = serde_json::from_value(serde_json::json!({"schema_version":1,"applications":{"available":true,"total":1,
            "packages":[{"scope":"system","name":"editor","version":"1.2","desktop_ids":["editor.desktop"]}]},"tools":[]})).unwrap();
        *service.software.lock().await = Some(software::Cache {
            store: software::store::Store::open(&directory.path().join("db/catalog.db")).unwrap(),
            observations,
            attempted: Some(Instant::now()),
            worker: "/nonexistent/worker".into(),
        });
        let query = || QueryParams {
            query: String::new(),
            category: String::new(),
            generation: 1,
            limit: 100,
        };
        let page = service.query(query()).await.unwrap();
        let catalog = service.software_catalog(false).await.unwrap();
        assert_eq!(
            page.applications[0].software.as_ref(),
            Some(&catalog.entries[0])
        );
        service
            .update_settings(super::super::UpdateSettingsParams {
                target_id: "editor.desktop".into(),
                category: Some(Some("text".into())),
                new_workspace: Some(true),
                autostart: None,
            })
            .await
            .unwrap();
        let page = service.query(query()).await.unwrap();
        let catalog = service.software_catalog(false).await.unwrap();
        assert_eq!(page.applications[0].identity.category, "text");
        assert_eq!(
            page.applications[0].software.as_ref(),
            Some(&catalog.entries[0])
        );
        assert_eq!(catalog.entries[0].category, "text");
        assert_eq!(
            catalog.entries[0].installations[0]
                .declared_version
                .as_deref(),
            Some("1.2")
        );
        assert_eq!(service.revision().await, page.revision);
        service.shutdown().await;
    }
}
