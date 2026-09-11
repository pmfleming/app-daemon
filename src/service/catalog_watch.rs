use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

pub(super) enum CatalogEvent {
    Changed,
    Rewatch,
    Failed,
}

pub(super) fn create(sender: mpsc::Sender<CatalogEvent>) -> notify::Result<RecommendedWatcher> {
    let roots = crate::catalog::default_catalog_paths();
    let plan = watch_plan(&roots);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) => {
                if event.kind.is_access() {
                    return;
                }
                if !event.paths.iter().any(|path| relevant(path, &roots)) {
                    return;
                }
                if event
                    .paths
                    .iter()
                    .any(|path| roots.iter().any(|root| root.starts_with(path)))
                {
                    CatalogEvent::Rewatch
                } else {
                    CatalogEvent::Changed
                }
            }
            Err(error) => {
                tracing::warn!(%error, "catalog watcher failed; rebuilding watch");
                CatalogEvent::Failed
            }
        };
        let _ = sender.try_send(event);
    })?;
    for (path, mode) in plan {
        // Partial coverage is not healthy: retry installation while polling.
        watcher.watch(&path, mode)?;
    }
    Ok(watcher)
}

fn relevant(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| path.starts_with(root) || root.starts_with(path))
}

fn watch_plan(roots: &[PathBuf]) -> HashMap<PathBuf, RecursiveMode> {
    let mut plan = HashMap::new();
    for root in roots {
        if root.is_dir() {
            plan.insert(root.clone(), RecursiveMode::Recursive);
        }
        // Parent watches notice directory creation/replacement. Watch parents of
        // symlink ancestors too, so a Nix profile switch cannot strand us on the
        // old store directory until the slow reconciliation timer.
        for ancestor in root.ancestors() {
            let symlink = ancestor
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink());
            if (ancestor == root || symlink)
                && let Some(parent) = ancestor
                    .parent()
                    .and_then(|p| p.ancestors().find(|p| p.is_dir()))
            {
                plan.entry(parent.to_owned())
                    .or_insert(RecursiveMode::NonRecursive);
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn watches_missing_roots_and_profile_symlinks_without_watching_all_home_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store/share/applications");
        std::fs::create_dir_all(&store).unwrap();
        std::os::unix::fs::symlink(dir.path().join("store"), dir.path().join("profile")).unwrap();
        let roots = vec![
            dir.path().join("profile/share/applications"),
            dir.path().join("missing/applications"),
        ];
        let plan = watch_plan(&roots);
        assert!(matches!(
            plan.get(dir.path()),
            Some(RecursiveMode::NonRecursive)
        ));
        assert!(matches!(
            plan.get(&roots[0]),
            Some(RecursiveMode::Recursive)
        ));
        assert!(relevant(&dir.path().join("profile"), &roots));
        assert!(relevant(&dir.path().join("missing"), &roots));
        assert!(!relevant(&dir.path().join("unrelated-download"), &roots));
    }
}
