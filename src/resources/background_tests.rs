use super::{ResourceSampler, provider::ProcessStat, test_provider::TestProvider};
use crate::{
    catalog::Catalog,
    history::{HistoryStore, now_milliseconds},
    hyprland::Snapshot,
    service::query::{QueryParams, page},
    settings::SettingsStore,
};
use std::{collections::HashMap, sync::Arc, time::Instant};

fn process(parent: u32, start: u64) -> ProcessStat {
    ProcessStat {
        parent_pid: parent,
        start_ticks: start,
        total_ticks: 10,
        major_faults: 0,
        thread_count: 1,
    }
}

fn catalog() -> anyhow::Result<(tempfile::TempDir, Catalog)> {
    let dir = tempfile::tempdir()?;
    for (id, shortcut) in [
        ("org.example.App", false),
        ("org.example.Other", false),
        ("org.example.Shortcut", true),
    ] {
        std::fs::write(
            dir.path().join(format!("{id}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={id}\nExec=true\nX-Shelllist-LaunchOnly={shortcut}\n"
            ),
        )?;
    }
    let catalog = Catalog::from_paths(vec![dir.path().into()]);
    Ok((dir, catalog))
}

#[test]
fn windowless_apps_are_discovered_published_and_added_to_history() -> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        state.processes = HashMap::from([
            (42, process(1, 100)),
            (43, process(42, 101)),
            (99, process(1, 1)),
        ]);
        state.cgroups = HashMap::from([
            (
                42,
                "/user.slice/app-org.example.App@12345678.service".into(),
            ),
            (
                43,
                "/user.slice/app-org.example.App@12345678.service".into(),
            ),
            (
                99,
                "/user.slice/app-org.example.Shortcut@12345678.service".into(),
            ),
        ]);
        state.system_ticks = 100;
    }
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    sampler.previous_sample = Some(Instant::now() - std::time::Duration::from_secs(2));
    {
        let mut state = provider.state.lock().unwrap();
        state.system_ticks += 100;
        state.processes.get_mut(&42).unwrap().total_ticks += 20;
    }
    let snapshot = sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    assert_eq!(
        snapshot.target_roots.len(),
        1,
        "launch-only scopes must not claim resources"
    );
    let usage = snapshot.usage_for_application("org.example.App.desktop", []);
    assert_eq!(usage.compute.process_count, 2);
    assert_eq!(usage.compute.cpu_percent, 20.0);
    let result = page(
        &catalog,
        &Snapshot::default(),
        &snapshot,
        &SettingsStore::load(None),
        &QueryParams {
            query: String::new(),
            category: String::new(),
            generation: 0,
            limit: 100,
        },
        HashMap::new(),
        0,
    );
    let app = result
        .applications
        .iter()
        .find(|app| app.identity.id == "org.example.App.desktop")
        .unwrap();
    assert!(app.runtime.running);
    assert_eq!(app.runtime.running_count, 0);
    assert_eq!(app.runtime.resources.compute.process_count, 2);
    let mut history = HistoryStore::load(None);
    for (id, roots) in snapshot.target_roots() {
        history.record(
            id,
            now_milliseconds() - 30_000,
            snapshot.interval_seconds(),
            &snapshot.usage_for_target(id, roots.iter().copied()),
        );
    }
    assert_eq!(
        history
            .query("org.example.App.desktop", None, None, 100)?
            .points
            .len(),
        1
    );
    Ok(())
}

#[test]
fn ownership_follows_window_reassignment_and_survives_parent_exit_but_not_pid_reuse()
-> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().processes = HashMap::from([(42, process(1, 100))]);
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let windows = HashMap::from([("org.example.App.desktop".into(), vec![42])]);
    sampler.sample_for_applications(&windows, &catalog, &Default::default());
    let windows = HashMap::from([("org.example.Other.desktop".into(), vec![42])]);
    let reassigned = sampler.sample_for_applications(&windows, &catalog, &Default::default());
    assert_eq!(reassigned.target_roots(), &windows);
    provider
        .state
        .lock()
        .unwrap()
        .processes
        .insert(43, process(42, 101));
    sampler.sample_for_applications(&windows, &catalog, &Default::default());
    provider.state.lock().unwrap().processes.remove(&42);
    provider
        .state
        .lock()
        .unwrap()
        .processes
        .get_mut(&43)
        .unwrap()
        .parent_pid = 1;
    let background =
        sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    assert_eq!(
        background
            .usage_for_application("org.example.Other.desktop", [])
            .compute
            .process_count,
        1
    );
    provider
        .state
        .lock()
        .unwrap()
        .processes
        .get_mut(&43)
        .unwrap()
        .start_ticks = 999;
    let reused = sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    assert!(reused.target_roots().is_empty());
    assert_ne!(background.runtime_revision(), reused.runtime_revision());
    Ok(())
}

#[test]
fn unrelated_launcher_groups_are_not_borrowed_and_stable_dbus_units_are_discovered()
-> anyhow::Result<()> {
    let (dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        state.processes = HashMap::from([(42, process(1, 100)), (43, process(1, 101))]);
        state.cgroups = HashMap::from([
            (42, "/app-launcher.service".into()),
            (43, "/app-launcher.service".into()),
        ]);
    }
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let windows = HashMap::from([("org.example.App.desktop".into(), vec![42])]);
    let snapshot = sampler.sample_for_applications(&windows, &catalog, &Default::default());
    let usage = snapshot.usage_for_application("org.example.App.desktop", []);
    assert_eq!(
        usage.compute.process_count, 1,
        "do not charge unrelated siblings in the launcher service"
    );
    assert_eq!(usage.measurement.attribution_method, "process-tree");
    std::fs::write(
        dir.path().join("org.example.Bus.desktop"),
        "[Desktop Entry]\nType=Application\nName=Bus\nDBusActivatable=true\n",
    )?;
    let catalog = Catalog::from_paths(vec![dir.path().into()]);
    provider
        .state
        .lock()
        .unwrap()
        .cgroups
        .insert(43, "/org.example.Bus.service/worker".into());
    let snapshot = sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    assert_eq!(
        snapshot
            .usage_for_application("org.example.Bus.desktop", [])
            .compute
            .process_count,
        1
    );
    assert_eq!(
        snapshot.cgroup_path_by_root[&43],
        "/org.example.Bus.service"
    );
    Ok(())
}

#[test]
fn separately_owned_child_applications_are_not_charged_to_the_parent() -> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        state.processes = HashMap::from([
            (42, process(1, 100)),
            (43, process(42, 101)),
            (44, process(43, 102)),
            (45, process(44, 103)),
            (46, process(45, 104)),
        ]);
        state.cgroups = HashMap::from([
            (42, "/app-org.example.App.service".into()),
            (43, "/app-org.example.Other.service".into()),
            (45, "/app-org.example.App.service/reclaimed".into()),
        ]);
    }
    let mut sampler = ResourceSampler {
        provider,
        ..Default::default()
    };
    let snapshot = sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    for (id, count) in [
        ("org.example.App.desktop", 3),
        ("org.example.Other.desktop", 2),
    ] {
        let usage = snapshot.usage_for_application(id, []);
        assert_eq!(usage.compute.process_count, count, "{id}");
        assert!(!usage.measurement.resources_shared);
    }
    Ok(())
}

#[test]
fn verified_migrated_ownership_unifies_resources_without_borrowing_whole_host_scopes()
-> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        state.processes = HashMap::from([
            (42, process(1, 100)),
            (43, process(42, 101)),
            (44, process(42, 102)),
            (50, process(1, 200)),
            (51, process(50, 201)),
            (99, process(1, 999)),
        ]);
        state.cgroups = HashMap::from([
            (42, "/app-org.example.Other-42.scope".into()),
            (43, "/app-org.example.App@123.service".into()),
            (44, "/app-org.example.Other-42.scope".into()),
            (50, "/app-org.example.Other-50.scope".into()),
            (51, "/app-org.example.Other-50.scope".into()),
            // Another process sharing the migrated scope is not a descendant.
            (99, "/app-org.example.Other-42.scope".into()),
        ]);
    }
    let mut ownership = crate::ownership::Ownership::default();
    ownership.remember("org.example.App.desktop", [(42, 100)]);
    let reconcile = |ownership: &mut crate::ownership::Ownership| {
        let state = provider.state.lock().unwrap();
        ownership.reconcile(
            &catalog,
            &state
                .processes
                .iter()
                .map(|(&pid, stat)| {
                    (
                        pid,
                        crate::process::Process {
                            parent: stat.parent_pid,
                            start: stat.start_ticks,
                            cgroup: state.cgroups.get(&pid).cloned(),
                        },
                    )
                })
                .collect(),
        );
    };
    reconcile(&mut ownership);
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    // Both launchers can exist in the catalog. A previous generic attribution
    // must disappear, not survive in KnownRoots and charge the same PID twice.
    sampler.sample_for_applications(&HashMap::new(), &catalog, &Default::default());
    let snapshot =
        sampler.sample_for_applications(&HashMap::new(), &catalog, &ownership.snapshot());
    let app = snapshot.usage_for_application("org.example.App.desktop", []);
    let host = snapshot.usage_for_application("org.example.Other.desktop", []);
    assert_eq!(app.compute.process_count, 3);
    assert_eq!(host.compute.process_count, 3);
    assert_eq!(app.measurement.attribution_method, "process-tree");
    assert!(!app.measurement.resources_shared);
    assert!(!host.measurement.resources_shared);
    // Window closure does not forget the app, and a late audio helper joins it.
    {
        let mut state = provider.state.lock().unwrap();
        state.processes.insert(45, process(42, 103));
        state
            .cgroups
            .insert(45, "/app-org.example.Other-42.scope".into());
    }
    reconcile(&mut ownership);
    let snapshot =
        sampler.sample_for_applications(&HashMap::new(), &catalog, &ownership.snapshot());
    assert_eq!(
        snapshot
            .usage_for_application("org.example.App.desktop", [])
            .compute
            .process_count,
        4
    );
    assert_eq!(
        snapshot
            .usage_for_application("org.example.Other.desktop", [])
            .compute
            .process_count,
        3
    );
    // Ambiguous verified roots and same-scope helpers are not charged to either
    // application. The early helper's independent App service still identifies it.
    ownership.remember("org.example.Other.desktop", [(42, 100)]);
    reconcile(&mut ownership);
    let snapshot =
        sampler.sample_for_applications(&HashMap::new(), &catalog, &ownership.snapshot());
    assert_eq!(
        snapshot
            .usage_for_application("org.example.App.desktop", [])
            .compute
            .process_count,
        1
    );
    assert_eq!(
        snapshot
            .usage_for_application("org.example.Other.desktop", [])
            .compute
            .process_count,
        3
    );
    Ok(())
}
