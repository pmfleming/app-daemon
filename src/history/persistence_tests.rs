use super::*;

// Deterministic shape; wall time only keeps all 24,000 buckets inside retention.
fn fixture() -> HistoryStore {
    let mut store = HistoryStore::load(None);
    let end = now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS;
    for app in 0..100 {
        store.insert_points(
            format!("fixture-{app}"),
            (0..240).map(|i| ResourceHistoryPoint {
                timestamp_ms: end - (240 - i) * BUCKET_MILLISECONDS,
                duration_ms: BUCKET_MILLISECONDS,
                resources: crate::model::HistoricalResourceUsage {
                    availability: Some(crate::model::ResourceAvailability {
                        cpu: true,
                        memory: true,
                        ..Default::default()
                    }),
                    coverage: 1.0,
                    sample_count: 3,
                    ..Default::default()
                },
            }),
        );
    }
    store
}

#[test]
fn compact_storage_ignores_legacy_maps_and_projects_only_responses() -> anyhow::Result<()> {
    let mut store = fixture();
    let snapshot = store.snapshot(false).unwrap();
    let mut json = serde_json::to_value(&snapshot.file)?;
    let point = &mut json["applications"]["fixture-0"][0];
    assert!(point.get("metric_availability").is_none());
    // A stale legacy projection must not override the canonical availability flags.
    point["metric_availability"] = serde_json::json!({"cpu_percent": false});
    let loaded: HistoryFile = serde_json::from_value(json)?;
    assert!(
        loaded
            .applications
            .values()
            .flatten()
            .all(|p| p.resources.metric_availability.is_empty())
    );
    let response = store.query_window("fixture-0", None, None, None, 1)?;
    assert!(response.points[0].resources.metric_availability.0["cpu_percent"]);
    assert!(!response.points[0].resources.metric_availability.0["gpu_percent"]);
    assert_eq!(response.points[0].resources.metric_availability.0.len(), 48);
    assert!(
        store
            .points
            .values()
            .flatten()
            .all(|p| p.resources.metric_availability.is_empty())
    );
    let old: ResourceHistoryPoint = serde_json::from_value(serde_json::json!({
        "timestamp_ms": 1, "duration_ms": 1, "coverage": 1.0,
        "metric_availability": {"gpu_percent": true}
    }))?;
    let old = summary::normalize(old);
    assert!(old.resources.metric_availability.0["cpu_percent"]);
    assert!(!old.resources.metric_availability.0["gpu_percent"]);
    Ok(())
}

#[test]
fn snapshots_share_finished_buckets_but_mutations_do_not_change_the_snapshot() {
    let mut store = fixture();
    let snapshot = store.snapshot(false).unwrap();
    let old = snapshot.file.applications["fixture-0"][0].clone();
    assert!(Arc::ptr_eq(&old, &store.points["fixture-0"][0]));
    let mut replacement = (*old).clone();
    replacement.resources.compute.cpu_percent = 90.0;
    store.insert_points("fixture-0".into(), [replacement]);
    assert!(!Arc::ptr_eq(&old, &store.points["fixture-0"][0]));
    assert_eq!(old.resources.compute.cpu_percent, 0.0);
    assert!(Arc::ptr_eq(
        &snapshot.file.applications["fixture-0"][1],
        &store.points["fixture-0"][1]
    ));
    store.insert_energy_points(
        "app".into(),
        [EnergyHistoryPoint {
            timestamp_ms: now_milliseconds(),
            energy_mwh: 1.0,
            ..Default::default()
        }],
    );
    let snapshot = store.snapshot(false).unwrap();
    let old = snapshot.file.energy_applications["app"][0].clone();
    store.insert_energy_points("app".into(), [(*old).clone()]);
    assert_eq!(old.energy_mwh, 1.0);
    assert_eq!(store.energy_points["app"][0].energy_mwh, 2.0);
}

#[test]
fn saves_track_successful_generations_and_flush_pending_on_shutdown() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state");
    let mut store = HistoryStore::load(Some(path.clone()));
    let first = store.snapshot(false).unwrap();
    let t = now_milliseconds() + 60_000;
    store.record("app", t, 1.0, &ResourceUsage::default());
    persist_snapshot(first)?;
    assert!(
        store.snapshot(false).is_none(),
        "pending data is not yet persisted state"
    );
    let final_save = store.snapshot(true).unwrap();
    store.record("other", t + 15_000, 1.0, &ResourceUsage::default());
    store.flush_pending();
    persist_snapshot(final_save)?;
    assert!(
        store.snapshot(false).is_some(),
        "an older save must not clear newer data"
    );
    persist_snapshot(store.snapshot(false).unwrap())?;
    assert!(store.snapshot(false).is_none());
    let mut loaded = HistoryStore::load(Some(path.clone()));
    assert_eq!(
        loaded
            .query_window("other", None, None, None, 10)?
            .points
            .len(),
        1
    );
    // A failed replacement does not mark the generation saved.
    store.prune(t + ENERGY_RETENTION_MILLISECONDS + RETENTION_MILLISECONDS);
    std::fs::remove_file(&path)?;
    std::fs::create_dir(&path)?;
    assert!(persist_snapshot(store.snapshot(false).unwrap()).is_err());
    assert!(store.snapshot(false).is_some());
    std::fs::remove_dir(&path)?;
    persist_snapshot(store.snapshot(false).unwrap())?;
    assert!(store.snapshot(true).is_none());
    Ok(())
}

#[test]
#[ignore = "release-only isolated persistence probe; no live state"]
fn profile_streamed_history_save() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = fixture();
    store.path = Some(dir.path().join("history"));
    let started = std::time::Instant::now();
    persist_snapshot(store.snapshot(false).unwrap())?;
    eprintln!(
        "streamed_save_ms={:.3} bytes={} {}",
        started.elapsed().as_secs_f64() * 1000.0,
        store.path.as_ref().unwrap().metadata()?.len(),
        std::fs::read_to_string("/proc/self/status")?
            .lines()
            .find(|l| l.starts_with("VmHWM:"))
            .unwrap()
    );
    for _ in 0..3 {
        assert!(store.snapshot(false).is_none());
    }
    Ok(())
}

#[test]
#[ignore = "release-only isolated persistence probe; no live state"]
fn profile_history_persistence() {
    let mut store = fixture();
    let start = std::time::Instant::now();
    let snapshot = store.snapshot(false).unwrap();
    let snapshot_time = start.elapsed();
    let start = std::time::Instant::now();
    let bytes = serde_json::to_vec(&snapshot.file).unwrap();
    eprintln!(
        "buckets=24000 snapshot_ms={:.3} encode_ms={:.3} bytes={} rss={}",
        snapshot_time.as_secs_f64() * 1000.0,
        start.elapsed().as_secs_f64() * 1000.0,
        bytes.len(),
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .unwrap()
    );
    std::hint::black_box((store, snapshot, bytes));
}
