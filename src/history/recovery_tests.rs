use super::*;

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
fn restart_merges_partial_buckets_and_invalidates_pre_rewrite_cursors() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.json");
    let t = now_milliseconds() / ENERGY_BUCKET_MILLISECONDS * ENERGY_BUCKET_MILLISECONDS - 120_000;
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", t + 1000, 1.0, &usage(10.0, 1.0));
    persist_snapshot(store.snapshot(true))?;
    let cursor = store.query("app", None, None, 1)?.next_cursor;
    let mut restarted = HistoryStore::load(Some(path));
    assert!(
        restarted
            .query("app", None, cursor.as_deref(), 1)?
            .points
            .is_empty(),
        "normal restart preserves cursor validity"
    );
    restarted.record("app", t + 2000, 2.0, &usage(40.0, 2.0));
    restarted.snapshot(true);
    assert!(
        restarted.query("app", None, cursor.as_deref(), 1).is_err(),
        "rewritten buckets require an explicit resync"
    );
    let page = restarted.query("app", None, None, 1)?;
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
    assert_eq!(restarted.energy_points["app"].len(), 1);
    Ok(())
}

#[test]
fn backward_clock_changes_remain_sorted_and_cannot_silently_skip_backfills() -> anyhow::Result<()> {
    let mut store = HistoryStore::load(None);
    let t = now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS - 60_000;
    store.record("app", t, 1.0, &usage(10.0, 1.0));
    let cursor = store.query("app", None, None, 1)?.next_cursor;
    store.record("app", t - 30_000, 1.0, &usage(20.0, 1.0));
    assert!(store.query("app", None, cursor.as_deref(), 10).is_err());
    let first = store.query("app", None, None, 1)?;
    assert!(first.has_more);
    let second = store.query("app", None, first.next_cursor.as_deref(), 1)?;
    assert!(first.points[0].timestamp_ms < second.points[0].timestamp_ms);
    store.prune(t + RETENTION_MILLISECONDS + BUCKET_MILLISECONDS + 1);
    assert!(store.points.is_empty());
    Ok(())
}

#[test]
fn old_unsorted_duplicate_files_are_normalized_on_load() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.json");
    let t = now_milliseconds() - 60_000;
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", t, 1.0, &usage(10.0, 1.0));
    let mut snapshot = store.snapshot(true);
    let points = snapshot.file.applications.get_mut("app").unwrap();
    let mut older = points[0].clone();
    older.timestamp_ms -= BUCKET_MILLISECONDS;
    points.push_back(older.clone());
    points.push_back(older);
    persist_snapshot(snapshot)?;
    let mut loaded = HistoryStore::load(Some(path));
    let page = loaded.query("app", None, None, 10)?;
    assert_eq!(page.points.len(), 2);
    assert!(page.points[0].timestamp_ms < page.points[1].timestamp_ms);
    assert_eq!(page.points[0].duration_ms, 2000);
    Ok(())
}
