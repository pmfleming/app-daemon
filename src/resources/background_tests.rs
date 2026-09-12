use super::test_provider::TestProvider;
use super::*;
use crate::{
    catalog::Catalog,
    history::{HistoryStore, now_milliseconds},
    hyprland::Snapshot,
    service::query::{QueryParams, page},
    settings::SettingsStore,
};

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
    sampler.sample_for_applications(&HashMap::new(), &catalog);
    sampler.previous_sample = Some(Instant::now() - std::time::Duration::from_secs(2));
    {
        let mut state = provider.state.lock().unwrap();
        state.system_ticks += 100;
        state.processes.get_mut(&42).unwrap().total_ticks += 20;
    }
    let snapshot = sampler.sample_for_applications(&HashMap::new(), &catalog);
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
fn known_processes_survive_window_and_parent_exit_but_not_pid_reuse() -> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().processes =
        HashMap::from([(42, process(1, 100)), (43, process(42, 101))]);
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let windows = HashMap::from([("org.example.App.desktop".into(), vec![42])]);
    sampler.sample_for_applications(&windows, &catalog);
    provider.state.lock().unwrap().processes.remove(&42);
    provider
        .state
        .lock()
        .unwrap()
        .processes
        .get_mut(&43)
        .unwrap()
        .parent_pid = 1;
    let background = sampler.sample_for_applications(&HashMap::new(), &catalog);
    assert_eq!(
        background
            .usage_for_application("org.example.App.desktop", [])
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
    let reused = sampler.sample_for_applications(&HashMap::new(), &catalog);
    assert!(reused.target_roots().is_empty());
    assert_ne!(background.runtime_revision(), reused.runtime_revision());
    Ok(())
}

#[test]
fn separately_owned_child_applications_are_not_charged_to_the_parent() -> anyhow::Result<()> {
    let (_dir, catalog) = catalog()?;
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        state.processes = HashMap::from([(42, process(1, 100)), (43, process(42, 101))]);
        state.cgroups = HashMap::from([
            (42, "/app-org.example.App.service".into()),
            (43, "/app-org.example.Other.service".into()),
        ]);
    }
    let mut sampler = ResourceSampler {
        provider,
        ..Default::default()
    };
    let snapshot = sampler.sample_for_applications(&HashMap::new(), &catalog);
    for id in ["org.example.App.desktop", "org.example.Other.desktop"] {
        let usage = snapshot.usage_for_application(id, []);
        assert_eq!(usage.compute.process_count, 1, "{id}");
        assert!(!usage.measurement.resources_shared);
    }
    Ok(())
}
