use super::{
    identity::resolve_target_with_cgroup,
    query::{QueryParams, page},
};
use crate::{
    benchmarks::{Measurement, measure},
    catalog::Catalog,
    hyprland::{Client, Snapshot, Workspace},
    resources::ResourceSnapshot,
    settings::SettingsStore,
};
use std::{collections::HashMap, fs, hint::black_box};

pub(crate) fn run(iterations: usize) -> anyhow::Result<Vec<Measurement>> {
    let directory = tempfile::tempdir()?;
    for index in 0..1000 {
        fs::write(
            directory.path().join(format!("app-{index:04}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName=Application {index:04}\nComment=Edit documents and media\nKeywords=editor;documents;\nCategories=Development;\nExec=true\n"
            ),
        )?;
    }
    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let windows = Snapshot {
        available: true,
        clients: (0..200)
            .map(|index| Client {
                address: format!("0x{index:x}"),
                class: format!("app-{index:04}"),
                initial_class: String::new(),
                title: format!("Document {index}"),
                pid: index + 1,
                workspace: Workspace::default(),
                focus_rank: i64::from(index),
                mapped: true,
            })
            .collect(),
        ..Default::default()
    };
    let resources = ResourceSnapshot::default();
    let settings = SettingsStore::load(None);
    // Pre-resolve ownership: page latency excludes procfs/compositor calls.
    let grouped: HashMap<_, _> = windows
        .clients
        .iter()
        .map(|window| (format!("{}.desktop", window.class), vec![window]))
        .collect();
    let mut measurements = Vec::new();
    for (name, query) in [("query/empty", ""), ("query/search", "app 004")] {
        let params = QueryParams {
            query: query.into(),
            category: String::new(),
            generation: 1,
            limit: 500,
        };
        measurements.push(measure(name, iterations, || {
            black_box(page(
                &catalog,
                &windows,
                &resources,
                &settings,
                &params,
                grouped.clone(),
                0,
            ));
        }));
    }
    measurements.push(measure("identity/no-cgroup", iterations, || {
        for window in &windows.clients {
            black_box(resolve_target_with_cgroup(&catalog, window, None));
        }
    }));
    Ok(measurements)
}
