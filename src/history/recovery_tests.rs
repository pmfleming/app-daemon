use super::{
    ENERGY_BUCKET_MILLISECONDS, HistoryStore, RETENTION_MILLISECONDS, ResourceUsage,
    now_milliseconds, persist_snapshot,
};

fn usage(cpu: f64, energy: f64) -> ResourceUsage {
    let mut usage = ResourceUsage::default();
    usage.compute.cpu_percent = cpu;
    usage.energy.energy_mwh = energy;
    usage.energy.energy_source = "rapl".into();
    usage.energy.energy_confidence = "low".into();
    usage.measurement.coverage = 1.0;
    usage
}

#[test]
fn cursor_lifecycle_survives_restart_but_rejects_expiry_rewrites_and_backfills()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.json");
    let t = now_milliseconds() / ENERGY_BUCKET_MILLISECONDS * ENERGY_BUCKET_MILLISECONDS - 120_000;
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record(
        "app",
        t - RETENTION_MILLISECONDS + 600_000,
        1.0,
        &usage(10.0, 1.0),
    );
    let oldest = store.query_window("app", None, None, None, 1)?;
    let old_cursor = oldest.next_cursor.as_deref().unwrap();
    let cutoff = oldest.points[0].timestamp_ms + RETENTION_MILLISECONDS;
    store.record("app", t + 1000, 1.0, &usage(10.0, 1.0));
    let recent = store.query_window("app", None, None, Some(old_cursor), 1)?;
    assert_eq!(recent.points.len(), 1, "normal appends preserve cursors");
    assert_eq!(
        store
            .query_window("app", Some(t), Some(t + 2000), Some(old_cursor), 1)?
            .points,
        recent.points,
        "an anchor outside the requested window remains valid"
    );
    assert!(
        store
            .query_window("other-app", None, None, Some(old_cursor), 1)
            .is_err()
    );
    store.prune(cutoff);
    assert!(
        store
            .query_window("app", None, None, Some(old_cursor), 1)
            .is_ok()
    );
    store.prune(cutoff + 1);
    assert!(
        store
            .query_window("app", None, None, Some(old_cursor), 1)
            .unwrap_err()
            .to_string()
            .contains("cursor expired")
    );
    let cursor = recent.next_cursor;
    let incremental = store.query_window("app", None, None, cursor.as_deref(), 1)?;
    assert!(incremental.points.is_empty());
    assert_eq!(incremental.next_cursor, cursor);
    persist_snapshot(store.snapshot(false))?;

    let mut restarted = HistoryStore::load(Some(path.clone()));
    assert!(
        restarted
            .query_window("app", None, None, Some(old_cursor), 1)
            .is_err()
    );
    assert!(
        restarted
            .query_window("app", None, None, cursor.as_deref(), 1)?
            .points
            .is_empty(),
        "normal restart preserves retained cursors"
    );
    restarted.record("app", t + 3000, 2.0, &usage(40.0, 2.0));
    restarted.snapshot(true);
    assert!(
        restarted
            .query_window("app", None, None, cursor.as_deref(), 1)
            .is_err(),
        "rewritten buckets require an explicit resync"
    );
    let page = restarted.query_window("app", None, None, None, 1)?;
    assert_eq!(page.points.len(), 1);
    assert!(!page.has_more);
    let point = &page.points[0];
    assert_eq!(point.duration_ms, 3000);
    assert_eq!(point.resources.sample_count, 2);
    assert_eq!(point.resources.compute.cpu_percent, 30.0);
    assert_eq!(point.resources.peaks.cpu_percent, 40.0);
    assert_eq!(point.resources.energy_mwh, 3.0);
    assert_eq!(
        restarted.energy_totals(t, now_milliseconds())[0].energy_mwh,
        3.0
    );
    restarted.record("app", t + 6000, 1.0, &usage(20.0, 1.0));
    persist_snapshot(restarted.snapshot(true))?;
    let mut restarted = HistoryStore::load(Some(path));
    let gap = restarted.query_window("app", Some(t + 3000), Some(t + 5000), None, 10)?;
    assert_eq!(gap.summary.metrics["cpu_percent_of_machine"].observed_ms, 0);
    // The 6000 endpoint contributes 500ms to the summary but not to pagination.
    let clipped = restarted.query_window("app", Some(t), Some(t + 5500), None, 1)?;
    assert_eq!(clipped.points.len(), 1);
    assert!(!clipped.has_more);
    assert_eq!(
        clipped.summary.metrics["cpu_percent_of_machine"].observed_ms,
        3500
    );
    let exhausted = restarted.query_window(
        "app",
        Some(t),
        Some(t + 5500),
        clipped.next_cursor.as_deref(),
        1,
    )?;
    assert!(exhausted.points.is_empty());
    assert_eq!(exhausted.next_cursor, clipped.next_cursor);
    assert_eq!(exhausted.summary, clipped.summary);
    let cursor = restarted
        .query_window("app", None, None, None, 10)?
        .next_cursor;
    restarted.record("app", t - 30_000, 1.0, &usage(20.0, 1.0));
    assert!(
        restarted
            .query_window("app", None, None, cursor.as_deref(), 10)
            .is_err()
    );
    let points = restarted.query_window("app", None, None, None, 10)?.points;
    assert_eq!(points.len(), 3);
    assert!(
        points
            .windows(2)
            .all(|pair| pair[0].timestamp_ms < pair[1].timestamp_ms)
    );
    Ok(())
}
