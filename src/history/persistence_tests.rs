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
    let snapshot = store.snapshot(false);
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
#[ignore = "release-only isolated persistence probe; no live state"]
fn profile_history_persistence() {
    let mut store = fixture();
    let start = std::time::Instant::now();
    let snapshot = store.snapshot(false);
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
