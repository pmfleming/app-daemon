use super::{
    BUCKET_MILLISECONDS, ENERGY_BUCKET_MILLISECONDS, HistoryStore, RETENTION_MILLISECONDS,
    ResourceUsage, now_milliseconds, persist_snapshot,
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
fn restart_preserves_gaps_and_rewrites_or_clock_backfills_invalidate_cursors() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.json");
    let t = now_milliseconds() / ENERGY_BUCKET_MILLISECONDS * ENERGY_BUCKET_MILLISECONDS - 120_000;
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", t + 1000, 1.0, &usage(10.0, 1.0));
    persist_snapshot(store.snapshot(true))?;
    let cursor = store.query("app", None, None, 1)?.next_cursor;
    let mut restarted = HistoryStore::load(Some(path.clone()));
    assert!(
        restarted
            .query("app", None, cursor.as_deref(), 1)?
            .points
            .is_empty(),
        "normal restart preserves cursor validity"
    );
    restarted.record("app", t + 3000, 2.0, &usage(40.0, 2.0));
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
    restarted.record("app", t + 6000, 1.0, &usage(20.0, 1.0));
    persist_snapshot(restarted.snapshot(true))?;
    let mut restarted = HistoryStore::load(Some(path));
    let gap = restarted.query_window("app", Some(t + 3000), Some(t + 5000), None, 10)?;
    assert_eq!(gap.summary.metrics["cpu_percent_of_machine"].observed_ms, 0);
    let cursor = restarted.query("app", None, None, 10)?.next_cursor;
    restarted.record("app", t - 30_000, 1.0, &usage(20.0, 1.0));
    assert!(restarted.query("app", None, cursor.as_deref(), 10).is_err());
    let points = restarted.query("app", None, None, 10)?.points;
    assert_eq!(points.len(), 3);
    assert!(
        points
            .windows(2)
            .all(|pair| pair[0].timestamp_ms < pair[1].timestamp_ms)
    );
    Ok(())
}

#[test]
fn pruning_expires_only_cursors_whose_buckets_are_gone_even_after_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.json");
    // End samples on a bucket boundary so each one-second interval occupies
    // exactly one bucket, regardless of the wall clock when this test runs.
    let now = now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS;
    let old = now - RETENTION_MILLISECONDS + 60_000;
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", old, 1.0, &usage(10.0, 1.0));
    let first = store.query("app", None, None, 1)?;
    let old_cursor = first.next_cursor.as_deref().unwrap();
    let old_endpoint = first.points[0].timestamp_ms;

    store.record("app", now - 60_000, 1.0, &usage(20.0, 1.0));
    let latest = store.query("app", None, Some(old_cursor), 1)?;
    assert_eq!(latest.points.len(), 1, "normal appends preserve cursors");
    let recent_cursor = latest.next_cursor.as_deref().unwrap();
    let narrowed = store.query_window(
        "app",
        Some(latest.points[0].timestamp_ms),
        Some(now),
        Some(old_cursor),
        1,
    )?;
    assert_eq!(
        narrowed.points, latest.points,
        "anchor may be outside the requested window"
    );

    // Retention is inclusive at its cutoff; one millisecond later the old
    // bucket disappears while the same target continues producing new data.
    store.prune(old_endpoint + RETENTION_MILLISECONDS);
    assert!(store.query("app", None, Some(old_cursor), 1).is_ok());
    store.prune(old_endpoint + RETENTION_MILLISECONDS + 1);
    let error = store.query("app", None, Some(old_cursor), 1).unwrap_err();
    assert!(error.to_string().contains("cursor expired"));
    let incremental = store.query("app", None, Some(recent_cursor), 1)?;
    assert!(incremental.points.is_empty());
    assert_eq!(incremental.next_cursor, latest.next_cursor);
    assert_eq!(store.query("app", None, None, 10)?.points, latest.points);

    persist_snapshot(store.snapshot(false))?;
    let mut restarted = HistoryStore::load(Some(path));
    assert!(restarted.query("app", None, Some(old_cursor), 1).is_err());
    assert!(
        restarted
            .query("app", None, Some(recent_cursor), 1)?
            .points
            .is_empty()
    );
    Ok(())
}
