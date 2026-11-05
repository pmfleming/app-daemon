use super::{
    BUCKET_MILLISECONDS, ENERGY_BUCKET_MILLISECONDS, ENERGY_RETENTION_MILLISECONDS, HistoryStore,
    RETENTION_MILLISECONDS, block_on, database::Fault, now_milliseconds, summary,
};
use crate::model::{
    HistoricalResourceUsage, ResourceAvailability, ResourceHistoryPoint, ResourceUsage,
};

fn point(timestamp_ms: u64) -> ResourceHistoryPoint {
    ResourceHistoryPoint {
        timestamp_ms,
        duration_ms: BUCKET_MILLISECONDS,
        resources: HistoricalResourceUsage {
            availability: Some(ResourceAvailability {
                cpu: true,
                memory: true,
                ..Default::default()
            }),
            coverage: 1.0,
            sample_count: 3,
            ..Default::default()
        },
    }
}

fn usage() -> ResourceUsage {
    let mut usage = ResourceUsage::default();
    usage.energy.energy_mwh = 1.0;
    usage.energy.energy_source = "rapl".into();
    usage.energy.energy_confidence = "high".into();
    usage
}

fn row_count(store: &HistoryStore, table: &str) -> anyhow::Result<i64> {
    block_on(
        store
            .database
            .integer(&format!("SELECT COUNT(*) FROM {table}")),
    )
}

#[test]
fn append_is_incremental_idle_flush_does_not_write_and_restart_does_not_load_points()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.db");
    let mut store = HistoryStore::open(&path)?;
    let end = now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS;
    store.insert_points(
        "app".into(),
        (0..1000).map(|i| point(end - (1000 - i) * BUCKET_MILLISECONDS)),
    )?;
    let before = block_on(store.database.integer("SELECT total_changes()"))?;
    store.insert_points("app".into(), [point(end)])?;
    assert_eq!(
        block_on(store.database.integer("SELECT total_changes()"))? - before,
        1
    );
    let before = block_on(store.database.integer("SELECT total_changes()"))?;
    store.flush(false)?;
    store.flush(false)?;
    assert_eq!(
        block_on(store.database.integer("SELECT total_changes()"))?,
        before
    );
    let page = store.query_window("app", None, Some(end), None, 1)?;
    assert!(page.has_more);
    assert_eq!(page.points[0].resources.metric_availability.0.len(), 48);
    let cursor = page.next_cursor;
    drop(store); // Completed buckets were committed without a periodic/final save.
    let mut reopened = HistoryStore::open(&path)?;
    assert!(reopened.pending.resources.is_empty() && reopened.pending.energy.is_empty());
    assert_eq!(
        block_on(
            reopened
                .database
                .integer("SELECT COUNT(*) FROM resource_points")
        )?,
        1001
    );
    let next = reopened.query_window("app", None, Some(end), cursor.as_deref(), 1)?;
    assert!(next.points[0].timestamp_ms > page.points[0].timestamp_ms);
    assert_eq!(next.summary, page.summary);
    Ok(())
}

#[test]
fn transaction_rollback_preserves_pending_and_retry_does_not_double_count() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.db");
    let mut store = HistoryStore::open(&path)?;
    let t = (now_milliseconds() / ENERGY_BUCKET_MILLISECONDS + 2) * ENERGY_BUCKET_MILLISECONDS;
    store.record("app", t + 1000, 1.0, &usage())?;
    store.database.fault = Some(Fault::Write);
    assert!(store.flush(true).is_err());
    assert_eq!(store.pending.resources.len(), 1);
    assert_eq!(store.pending.energy.len(), 1);
    assert_eq!(row_count(&store, "resource_points")?, 0);
    assert_eq!(row_count(&store, "energy_points")?, 0);
    assert_eq!(row_count(&store, "cursor_epochs")?, 0);
    store.flush(true)?;
    store.flush(true)?;
    drop(store);
    let mut store = HistoryStore::open(&path)?;
    assert_eq!(
        store
            .query_window("app", None, None, None, 10)?
            .points
            .len(),
        1
    );
    assert_eq!(store.energy_totals(t, t + 60_000)?[0].energy_mwh, 1.0);
    Ok(())
}

#[test]
fn uncertain_commit_and_failed_rollback_fence_further_reads_and_writes() -> anyhow::Result<()> {
    for fault in [Fault::Commit, Fault::Rollback] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history.db");
        let mut store = HistoryStore::open(&path)?;
        let t = (now_milliseconds() / 60_000 + 2) * 60_000;
        store.record("app", t + 1000, 1.0, &usage())?;
        store.database.fault = Some(fault);
        assert!(store.flush(true).is_err());
        assert!(
            store
                .flush(true)
                .unwrap_err()
                .to_string()
                .contains("fenced")
        );
        assert!(store.query_window("app", None, None, None, 10).is_err());
        assert!(store.energy_totals(t, t + 60_000).is_err());
        assert!(store.record("app", t + 2000, 1.0, &usage()).is_err());
        drop(store);
        let mut store = HistoryStore::open(&path)?;
        let points = store.query_window("app", None, None, None, 10)?.points;
        assert_eq!(points.len(), usize::from(fault == Fault::Commit));
        if fault == Fault::Commit {
            assert_eq!(store.energy_totals(t, t + 60_000)?[0].energy_mwh, 1.0);
        }
    }
    Ok(())
}

#[test]
fn retention_deletes_resource_and_energy_ranges_inclusively_at_boundary() -> anyhow::Result<()> {
    let mut store = HistoryStore::load(None);
    let now = now_milliseconds();
    // Avoid automatic retention while installing boundary fixtures.
    store.database.last_prune = Some(now);
    store.insert_points(
        "app".into(),
        [
            point(now - RETENTION_MILLISECONDS - 1),
            point(now - RETENTION_MILLISECONDS),
        ],
    )?;
    block_on(
        store.database.apply(
            super::FinishedBuckets {
                energy: [-1_i64, 0, 1]
                    .into_iter()
                    .map(|offset| {
                        (
                            "app".into(),
                            super::EnergyHistoryPoint {
                                timestamp_ms: (now - ENERGY_RETENTION_MILLISECONDS)
                                    .checked_add_signed(offset)
                                    .unwrap(),
                                energy_mwh: 1.0,
                                energy_source: "rapl".into(),
                                energy_confidence: "high".into(),
                            },
                        )
                    })
                    .collect(),
                ..Default::default()
            },
            now,
            false,
        ),
    )?;
    store.prune(now)?;
    assert_eq!(row_count(&store, "resource_points")?, 1);
    assert_eq!(row_count(&store, "energy_points")?, 2);
    assert_eq!(row_count(&store, "cursor_epochs")?, 1);
    store.prune(now + 2)?;
    assert_eq!(row_count(&store, "resource_points")?, 0);
    assert_eq!(row_count(&store, "energy_points")?, 0);
    assert_eq!(row_count(&store, "cursor_epochs")?, 0);
    Ok(())
}

#[test]
fn sql_summaries_match_reference_without_decoding_unpaged_payloads() -> anyhow::Result<()> {
    let mut store = HistoryStore::load(None);
    let t = now_milliseconds() / 15_000 * 15_000;
    let points: Vec<_> = (0..100)
        .map(|i| {
            let mut p = point(t - (100 - i) * 15_000);
            p.resources.compute.cpu_percent_of_machine = (i % 7) as f64;
            p.resources.peaks.cpu_percent_of_machine = 10.0;
            p.resources.availability.as_mut().unwrap().cpu = i % 3 != 0;
            p
        })
        .collect();
    store.insert_points("app".into(), points.clone())?;
    let epoch = block_on(store.database.epoch("app"))?;
    for (start, end) in [
        (t - 1_500_000, t),
        (t - 1_499_500, t - 900_001),
        (t - 400_000, t - 400_000),
    ] {
        let selected: Vec<_> = points
            .iter()
            .filter(|p| {
                p.timestamp_ms >= start
                    && (p.timestamp_ms <= end || p.timestamp_ms - p.duration_ms < end)
            })
            .collect();
        let expected = summary::summarize(selected, start, end, &epoch);
        let mut actual = store
            .query_window("app", Some(start), Some(end), None, 1)?
            .summary;
        // SQL's weighted sum and the reference's online mean differ by a few ULPs.
        for (name, stats) in &mut actual.metrics {
            if let (Some(actual), Some(expected)) = (stats.mean, expected.metrics[name].mean) {
                assert!((actual - expected).abs() <= 1e-10);
                stats.mean = Some(expected);
            }
        }
        assert_eq!(actual, expected);
    }
    // Keep explicit energy expectations on the production typed-SQL path.
    // Nonfinite JSON numbers cannot round-trip as f64; summaries do not decode them.
    {
        let sql_energy_summary = |points: &[&ResourceHistoryPoint],
                                  start,
                                  end|
         -> anyhow::Result<summary::HistorySummary> {
            let mut store = HistoryStore::load(None);
            store.insert_points("energy".into(), points.iter().copied().cloned())?;
            block_on(summary::sql_summary(
                &store.database.connection,
                "energy",
                start,
                start,
                end,
                "test",
            ))
        };
        let mut first = point(t + 1000);
        first.duration_ms = 1000;
        first.resources.availability.as_mut().unwrap().energy = true;
        first.resources.energy_source = "rapl".into();
        first.resources.energy_confidence = "high".into();
        first.resources.average_power_watts = 3.6;
        let mut second = first.clone();
        second.timestamp_ms = t + 3000;
        second.resources.energy_confidence = "low".into();
        let summary = sql_energy_summary(&[&first, &second], t + 500, t + 2500)?;
        assert_eq!(
            summary.metrics["average_power_watts"].observed_total,
            Some(1.0)
        );
        assert_eq!(
            summary.metrics["average_power_watts"].total_unit.as_deref(),
            Some("mWh")
        );
        assert_eq!(summary.energy_confidence, "low");
        second.resources.energy_source = "battery".into();
        let summary = sql_energy_summary(&[&first, &second], t + 500, t + 2500)?;
        assert_eq!(
            summary.metrics["average_power_watts"].observed_total,
            Some(0.5)
        );
        assert_eq!(summary.energy_confidence, "high");
        for power in [f64::NAN, f64::INFINITY, -1.0] {
            first.resources.average_power_watts = power;
            let summary = sql_energy_summary(&[&first], t, t + 1000)?;
            assert_eq!(summary.energy_confidence, "unknown");
            assert_eq!(summary.metrics["average_power_watts"].observed_total, None);
        }
        first.resources.average_power_watts = 0.0;
        assert_eq!(
            sql_energy_summary(&[&first], t, t + 1000)?.metrics["average_power_watts"]
                .observed_total,
            Some(0.0)
        );
    }
    // The summary uses typed columns, not 100 JSON decodes. A bad payload outside
    // this page does not erase history; fetching that row reports the corruption.
    block_on(store.database.connection.execute(
        "UPDATE resource_points SET payload='bad json' WHERE target_id='app' AND timestamp_ms=?",
        [super::storage::timestamp(points[99].timestamp_ms)?],
    ))?;
    assert!(store.query_window("app", None, None, None, 1).is_ok());
    assert!(
        store
            .query_window("app", Some(points[99].timestamp_ms), None, None, 1)
            .is_err()
    );
    assert_eq!(row_count(&store, "resource_points")?, 100);
    Ok(())
}

#[test]
fn range_queries_use_indexes() -> anyhow::Result<()> {
    let store = HistoryStore::load(None);
    for (sql, expected) in [
        (
            "EXPLAIN QUERY PLAN SELECT payload FROM resource_points WHERE target_id='app' AND timestamp_ms>=1 AND timestamp_ms<=100 ORDER BY timestamp_ms LIMIT 11",
            "INDEX",
        ),
        (
            "EXPLAIN QUERY PLAN DELETE FROM resource_points WHERE timestamp_ms<100",
            "resource_retention",
        ),
        (
            "EXPLAIN QUERY PLAN DELETE FROM energy_points WHERE timestamp_ms<100",
            "energy_retention",
        ),
    ] {
        let details = block_on(async {
            let mut rows = store.database.connection.query(sql, ()).await?;
            let mut details = String::new();
            while let Some(row) = rows.next().await? {
                details.push_str(&row.get::<String>(3)?);
            }
            anyhow::Ok(details)
        })?;
        assert!(details.contains(expected), "{details}");
        assert!(!details.contains("SCAN resource_points"), "{details}");
    }
    Ok(())
}

#[test]
fn unsafe_incompatible_and_corrupt_stores_fail_closed_and_legacy_json_is_untouched()
-> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let legacy = dir.path().join("resource-history-v1.json");
    std::fs::write(&legacy, b"old even if corrupt")?;
    let path = dir.path().join("resource-history-turso-v1.db");
    let store = HistoryStore::open(&path)?;
    assert!(HistoryStore::open(&path).is_err(), "only one owner");
    assert_eq!(path.metadata()?.permissions().mode() & 0o777, 0o600);
    assert_eq!(dir.path().metadata()?.permissions().mode() & 0o777, 0o700);
    drop(store);
    assert_eq!(std::fs::read(&legacy)?, b"old even if corrupt");
    let link = dir.path().join("link.db");
    std::os::unix::fs::symlink(&path, &link)?;
    assert!(HistoryStore::open(&link).is_err());
    let empty = dir.path().join("empty.db");
    std::fs::write(&empty, b"")?;
    assert!(HistoryStore::open(&empty).is_err());
    assert_eq!(empty.metadata()?.len(), 0);
    let bad = dir.path().join("bad.db");
    let bad_bytes = vec![b'x'; 4096];
    std::fs::write(&bad, &bad_bytes)?;
    assert!(HistoryStore::open(&bad).is_err());
    assert_eq!(std::fs::read(&bad)?, bad_bytes);
    let store = HistoryStore::open(&path)?;
    block_on(
        store
            .database
            .connection
            .execute("PRAGMA user_version=99", ()),
    )?;
    drop(store);
    assert!(HistoryStore::open(&path).is_err());
    Ok(())
}

#[test]
fn process_exit_recovers_committed_wal_without_final_save() -> anyhow::Result<()> {
    const CHILD: &str = "APP_DAEMON_HISTORY_CRASH_FIXTURE";
    if let Some(path) = std::env::var_os(CHILD) {
        let mut store = HistoryStore::open(std::path::Path::new(&path))?;
        let t = now_milliseconds() / 60_000 * 60_000 - 60_000;
        store.record("app", t + 15_000, 1.0, &usage())?;
        store.record("app", t + 16_000, 1.0, &usage())?;
        // Deliberately skip Rust destructors, final flush and checkpoint.
        std::process::exit(0);
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.db");
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "history::persistence_tests::process_exit_recovers_committed_wal_without_final_save",
            "--test-threads=1",
        ])
        .env(CHILD, &path)
        .status()?;
    assert!(status.success());
    assert!(dir.path().join("history.db-wal").metadata()?.len() > 0);
    let mut store = HistoryStore::open(&path)?;
    assert_eq!(
        store
            .query_window("app", None, None, None, 10)?
            .points
            .len(),
        1
    );
    assert_eq!(row_count(&store, "energy_points")?, 0);
    assert!(store.pending.resources.is_empty());
    Ok(())
}

#[test]
#[ignore = "isolated release storage probe; no live state"]
fn profile_turso_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = HistoryStore::open(&dir.path().join("history.db"))?;
    let end = now_milliseconds() / 15_000 * 15_000;
    let started = std::time::Instant::now();
    for app in 0..10 {
        store.insert_points(
            format!("app-{app}"),
            (0..3500).map(|i| point(end - (3500 - i) * 15_000)),
        )?;
    }
    let seed = started.elapsed();
    let started = std::time::Instant::now();
    let before = block_on(store.database.integer("SELECT total_changes()"))?;
    block_on(
        store.database.apply(
            super::FinishedBuckets {
                resources: (0..10)
                    .map(|app| (format!("app-{app}"), point(end)))
                    .collect(),
                ..Default::default()
            },
            now_milliseconds(),
            false,
        ),
    )?;
    let append = started.elapsed();
    let started = std::time::Instant::now();
    let page = store.query_window("app-0", None, Some(end), None, 100)?;
    eprintln!(
        "rows=35000 seed_ms={:.3} append_10_ms={:.3} changed_rows={} query_ms={:.3} page_rows={}",
        seed.as_secs_f64() * 1000.0,
        append.as_secs_f64() * 1000.0,
        block_on(store.database.integer("SELECT total_changes()"))? - before,
        started.elapsed().as_secs_f64() * 1000.0,
        page.points.len()
    );
    Ok(())
}
