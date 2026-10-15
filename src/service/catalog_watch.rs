use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

// Batch priority: failure must win over rewatch, which wins over invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CatalogEvent {
    Changed,
    Rewatch,
    Failed,
}

pub(super) fn create(sender: mpsc::Sender<CatalogEvent>) -> notify::Result<RecommendedWatcher> {
    let roots = crate::catalog::default_catalog_paths();
    let plan = watch_plan(&roots);
    let watcher = notify::recommended_watcher(move |event| {
        if let Some(event) = classify_event(event, &roots) {
            let _ = sender.try_send(event);
        }
    })?;
    install(watcher, plan)
}

fn install<W: Watcher>(
    mut watcher: W,
    plan: impl IntoIterator<Item = (PathBuf, RecursiveMode)>,
) -> notify::Result<W> {
    for (path, mode) in plan {
        // Partial coverage is not healthy: drop the watcher and retry while polling.
        watcher.watch(&path, mode)?;
    }
    Ok(watcher)
}

fn classify_event(event: notify::Result<notify::Event>, roots: &[PathBuf]) -> Option<CatalogEvent> {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(%error, "catalog watcher failed; rebuilding watch");
            return Some(CatalogEvent::Failed);
        }
    };
    if event.kind.is_access() || !event.paths.iter().any(|path| relevant(path, roots)) {
        return None;
    }
    Some(
        if event
            .paths
            .iter()
            .any(|path| roots.iter().any(|root| root.starts_with(path)))
        {
            CatalogEvent::Rewatch
        } else {
            CatalogEvent::Changed
        },
    )
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
        for parent in replacement_parents(root) {
            plan.entry(parent.to_owned())
                .or_insert(RecursiveMode::NonRecursive);
        }
    }
    plan
}

fn replacement_parents(root: &Path) -> impl Iterator<Item = &Path> {
    root.ancestors()
        .filter(move |path| {
            *path == root
                || path
                    .symlink_metadata()
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
        .filter_map(|path| path.parent()?.ancestors().find(|parent| parent.is_dir()))
}

#[cfg(test)]
mod tests {
    use super::{RecursiveMode, install, relevant, watch_plan};

    #[test]
    fn watch_coverage_handles_profile_replacement_and_rejects_partial_installation()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = dir.path().join("store/share/applications");
        std::fs::create_dir_all(&store)?;
        std::os::unix::fs::symlink(dir.path().join("store"), dir.path().join("profile"))?;
        let roots = [
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
        assert!(!relevant(&dir.path().join("unrelated-download"), &roots));
        // Overlapping roots must not downgrade recursive coverage in either order.
        for roots in [
            [store.clone(), dir.path().into()],
            [dir.path().into(), store.clone()],
        ] {
            let plan = watch_plan(&roots);
            assert!(
                roots
                    .iter()
                    .all(|root| plan[root] == RecursiveMode::Recursive)
            );
        }
        let watcher = notify::recommended_watcher(|_| {})?;
        assert!(
            install(
                watcher,
                [
                    (store, RecursiveMode::Recursive),
                    (roots[1].clone(), RecursiveMode::Recursive),
                ]
            )
            .is_err(),
            "a valid first watch must not hide a failed second watch"
        );
        Ok(())
    }
}
