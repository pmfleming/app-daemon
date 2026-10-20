use super::{Ownership, Process, process_identity};
use crate::catalog::Catalog;
use std::collections::HashMap;

fn catalog() -> (tempfile::TempDir, Catalog) {
    let dir = tempfile::tempdir().unwrap();
    for id in ["pocket", "audible", "chrome", "other", "shortcut"] {
        std::fs::write(dir.path().join(format!("{id}.desktop")), format!(
            "[Desktop Entry]\nType=Application\nName={id}\nExec=true\nX-Shelllist-LaunchOnly={}\n", id == "shortcut"
        )).unwrap();
    }
    let catalog = Catalog::from_paths(vec![dir.path().into()]);
    (dir, catalog)
}

fn process(parent: u32, start: u64, cgroup: &str) -> Process {
    Process {
        parent,
        start,
        cgroup: Some(cgroup.into()),
    }
}

#[test]
fn ownership_survives_reparenting_but_expires_on_pid_reuse_and_catalog_removal() {
    let (_dir, mut catalog) = catalog();
    let mut tracker = Ownership::default();
    let (pid, start) = process_identity(std::process::id()).unwrap();
    let (root, child) = (pid + 1, pid + 2);
    let mut processes = HashMap::from([
        (root, process(0, 0, "/app-chrome-10.scope")),
        (pid, process(root, start, "/app-chrome-10.scope")),
    ]);
    tracker.remember("pocket.desktop", [(root, 0)]);
    tracker.reconcile(&catalog, &processes);
    processes.remove(&root);
    processes.get_mut(&pid).unwrap().parent = 0;
    processes.insert(child, process(pid, start + 1, "/app-chrome-10.scope"));
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().live_target(pid),
        Some(Some("pocket.desktop"))
    );
    assert_eq!(
        tracker.snapshot().target((child, start + 1)),
        Some(Some("pocket.desktop"))
    );
    processes.get_mut(&pid).unwrap().start = start + 2;
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((pid, start + 2)), None);
    // A cached claim cannot authorize a different live identity.
    tracker.remember("pocket.desktop", [(pid, start + 2)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((pid, start + 2)),
        Some(Some("pocket.desktop"))
    );
    assert_eq!(tracker.snapshot().live_target(pid), None);
    catalog.entries.retain(|entry| entry.id != "pocket.desktop");
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().entries().count(), 0);
}

#[test]
fn reused_parents_do_not_suppress_retained_roots_and_cycles_terminate() {
    let (_dir, catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(0, 100, "/unidentified")),
        (11, process(10, 101, "/unidentified")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.reconcile(&catalog, &processes);
    processes.insert(10, process(0, 200, "/unidentified"));
    processes.insert(12, process(11, 102, "/unidentified"));
    tracker.remember("other.desktop", [(10, 200)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((10, 200)),
        Some(Some("other.desktop"))
    );
    for identity in [(11, 101), (12, 102)] {
        assert_eq!(
            tracker.snapshot().target(identity),
            Some(Some("pocket.desktop"))
        );
    }
    // Malformed same-start ancestry must not loop or overwrite the explicit anchor.
    processes.insert(13, process(11, 101, "/unidentified"));
    processes.get_mut(&11).unwrap().parent = 13;
    processes.get_mut(&10).unwrap().parent = 13;
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((13, 101)),
        Some(Some("pocket.desktop"))
    );
    assert_eq!(
        tracker.snapshot().target((10, 200)),
        Some(Some("other.desktop"))
    );
}

#[test]
fn application_boundaries_override_retained_descendants_and_contain_conflicts() {
    let (_dir, catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(1, 100, "/app-chrome-10.scope")),
        (11, process(10, 101, "/app-chrome-10.scope")),
        (12, process(11, 102, "/unidentified")),
        (13, process(12, 103, "/unidentified")),
        (20, process(10, 200, "/app-chrome-10.scope")),
        (21, process(20, 201, "/app-chrome-10.scope")),
        (30, process(1, 300, "/app-shortcut@123.service")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.remember("audible.desktop", [(20, 200)]);
    tracker.remember("shortcut.desktop", [(30, 300)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((13, 103)),
        Some(Some("pocket.desktop"))
    );
    assert_eq!(
        tracker.snapshot().target((21, 201)),
        Some(Some("audible.desktop"))
    );
    assert_eq!(tracker.snapshot().target((30, 300)), None);

    processes.get_mut(&11).unwrap().cgroup = Some("/app-other@456.service".into());
    tracker.reconcile(&catalog, &processes);
    for pid in [11, 12, 13] {
        assert_eq!(
            tracker.snapshot().target((pid, processes[&pid].start)),
            None
        );
    }
    // Reparenting below a newly discovered foreign process is a boundary too.
    processes.insert(40, process(1, 99, "/app-other@456.service"));
    processes.get_mut(&11).unwrap().cgroup = Some("/app-chrome-10.scope".into());
    tracker.reconcile(&catalog, &processes);
    processes.get_mut(&12).unwrap().parent = 40;
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((12, 102)), None);
    assert_eq!(tracker.snapshot().target((13, 103)), None);

    processes.get_mut(&11).unwrap().cgroup = Some("/app-other@456.service".into());
    tracker.remember("other.desktop", [(20, 200)]);
    processes.insert(22, process(21, 202, "/app-chrome-10.scope"));
    tracker.reconcile(&catalog, &processes);
    for (pid, start) in [(20, 200), (21, 201), (22, 202)] {
        assert_eq!(tracker.snapshot().target((pid, start)), Some(None));
    }
    tracker.remember("other.desktop", [(10, 100)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((11, 101)),
        None,
        "conflicts stop at independent units"
    );
    assert_eq!(tracker.snapshot().target((12, 102)), None);
}
