use super::*;

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
fn migrated_instances_keep_early_and_late_helpers_without_claiming_browser_siblings() {
    let (_dir, catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(1, 100, "/app-pocket@123.service")),
        (11, process(10, 101, "/app-pocket@123.service")),
        (20, process(1, 200, "/app-chrome-20.scope")),
        (21, process(20, 201, "/app-chrome-20.scope")),
        (30, process(1, 300, "/app-audible@123.service")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.remember("audible.desktop", [(30, 300)]);
    tracker.reconcile(&catalog, &processes);
    processes.get_mut(&10).unwrap().cgroup = Some("/app-chrome-10.scope".into());
    processes.get_mut(&30).unwrap().cgroup = Some("/app-chrome-30.scope".into());
    processes.insert(12, process(10, 102, "/app-chrome-10.scope/helpers"));
    processes.insert(
        13,
        process(10, 103, "/app-chrome-10.scope/app-other@456.service"),
    );
    processes.insert(31, process(30, 301, "/app-chrome-30.scope"));
    // Even a same-scope sibling is not owned by ancestry.
    processes.insert(99, process(1, 999, "/app-chrome-10.scope"));
    assert!(tracker.reconcile(&catalog, &processes));
    let snapshot = tracker.snapshot();
    for pid in [10, 11, 12] {
        assert_eq!(
            snapshot.target((pid, processes[&pid].start)),
            Some(Some("pocket.desktop"))
        );
    }
    for pid in [30, 31] {
        assert_eq!(
            snapshot.target((pid, processes[&pid].start)),
            Some(Some("audible.desktop"))
        );
    }
    for pid in [13, 20, 21, 99] {
        assert_eq!(snapshot.target((pid, processes[&pid].start)), None);
    }
    assert!(!tracker.reconcile(&catalog, &processes));
}

#[test]
fn ownership_survives_reparenting_but_expires_on_pid_reuse_and_catalog_removal() {
    let (_dir, mut catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(1, 100, "/app-chrome-10.scope")),
        (11, process(10, 101, "/app-chrome-10.scope")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.reconcile(&catalog, &processes);
    processes.remove(&10);
    processes.get_mut(&11).unwrap().parent = 1;
    processes.insert(12, process(11, 102, "/app-chrome-10.scope"));
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((12, 102)),
        Some(Some("pocket.desktop"))
    );
    processes.get_mut(&11).unwrap().start = 999;
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((11, 999)), None);
    assert_eq!(tracker.snapshot().target((11, 101)), None);
    catalog.entries.retain(|entry| entry.id != "pocket.desktop");
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().entries().count(), 0);
}

#[test]
fn independent_application_boundaries_conflicting_claims_and_shortcuts_are_not_inherited() {
    let (_dir, catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(1, 100, "/app-chrome-10.scope")),
        (11, process(10, 101, "/app-other@456.service")),
        (12, process(11, 102, "/unidentified")),
        (20, process(10, 200, "/app-chrome-10.scope")),
        (21, process(20, 201, "/app-chrome-10.scope")),
        (30, process(1, 300, "/app-shortcut@123.service")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.remember("audible.desktop", [(20, 200)]);
    tracker.remember("shortcut.desktop", [(30, 300)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((11, 101)), None);
    assert_eq!(tracker.snapshot().target((12, 102)), None);
    assert_eq!(
        tracker.snapshot().target((21, 201)),
        Some(Some("audible.desktop"))
    );
    assert_eq!(tracker.snapshot().target((30, 300)), None);
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
        "conflict propagation stops at an independent unit"
    );
    assert_eq!(tracker.snapshot().target((12, 102)), None);
    // Remove that conflicting root claim before checking a later scope change.
    tracker
        .anchors
        .get_mut(&(10, 100))
        .unwrap()
        .remove("other.desktop");
    // A helper entering an independently named application unit loses its old claim.
    processes.get_mut(&10).unwrap().cgroup = Some("/app-pocket@123.service".into());
    processes.insert(13, process(10, 103, "/app-pocket@123.service"));
    tracker.reconcile(&catalog, &processes);
    processes.get_mut(&13).unwrap().cgroup = Some("/app-other@456.service".into());
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((13, 103)), None);
}

#[test]
fn retained_grandchildren_cannot_bypass_a_new_ancestor_application_boundary() {
    let (_dir, catalog) = catalog();
    let mut tracker = Ownership::default();
    let mut processes = HashMap::from([
        (10, process(1, 100, "/app-chrome-10.scope")),
        (11, process(10, 101, "/app-chrome-10.scope")),
        (12, process(11, 102, "/unidentified")),
        (13, process(12, 103, "/unidentified")),
    ]);
    tracker.remember("pocket.desktop", [(10, 100)]);
    tracker.reconcile(&catalog, &processes);
    assert_eq!(
        tracker.snapshot().target((13, 103)),
        Some(Some("pocket.desktop"))
    );
    processes.get_mut(&11).unwrap().cgroup = Some("/app-other@456.service".into());
    tracker.reconcile(&catalog, &processes);
    for pid in [11, 12, 13] {
        assert_eq!(
            tracker.snapshot().target((pid, processes[&pid].start)),
            None
        );
    }
    // Reparenting into a newly discovered foreign process is a boundary too.
    processes.insert(20, process(1, 99, "/app-other@456.service"));
    processes.get_mut(&11).unwrap().parent = 10;
    processes.get_mut(&11).unwrap().cgroup = Some("/app-chrome-10.scope".into());
    tracker.reconcile(&catalog, &processes);
    processes.get_mut(&12).unwrap().parent = 20;
    tracker.reconcile(&catalog, &processes);
    assert_eq!(tracker.snapshot().target((12, 102)), None);
    assert_eq!(tracker.snapshot().target((13, 103)), None);
}

#[test]
fn cached_window_ownership_requires_a_live_pid_identity() {
    let (_dir, catalog) = catalog();
    let pid = std::process::id();
    let identity = process_identity(pid).unwrap();
    let mut tracker = Ownership::default();
    tracker.remember("pocket.desktop", [identity]);
    tracker.reconcile(
        &catalog,
        &HashMap::from([(pid, process(1, identity.1, "/app-chrome-10.scope"))]),
    );
    assert_eq!(
        tracker.snapshot().live_target(pid),
        Some(Some("pocket.desktop"))
    );
    tracker.anchors.clear();
    tracker.snapshot.owners.clear();
    tracker.remember("pocket.desktop", [(pid, identity.1 + 1)]);
    tracker.reconcile(
        &catalog,
        &HashMap::from([(pid, process(1, identity.1 + 1, "/app-chrome-10.scope"))]),
    );
    assert_eq!(tracker.snapshot().live_target(pid), None);
}
