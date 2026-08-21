use anyhow::Context;

use super::HistoryStore;
use crate::model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage};

#[test]
fn ignores_unknown_and_future_history_formats() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.json");
    std::fs::write(&path, br#"{"version":99,"applications":{"app":[]}}"#)?;
    let mut unknown = HistoryStore::load(Some(path.clone()));
    assert!(unknown.query("app", None, None, 10)?.points.is_empty());
    std::fs::write(&path, b"not json")?;
    let mut malformed = HistoryStore::load(Some(path));
    assert!(malformed.query("app", None, None, 10)?.points.is_empty());
    Ok(())
}

#[test]
fn aggregates_and_persists_resource_buckets() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.json");
    let mut store = HistoryStore::load(Some(path.clone()));
    let usage = ResourceUsage {
        compute: ComputeUsage {
            cpu_percent: 50.0,
            cpu_percent_of_machine: 12.5,
            memory_bytes: 1024,
            gpu_percent: 25.0,
            gpu_memory_bytes: 2048,
            ..ComputeUsage::default()
        },
        storage: StorageUsage {
            disk_read_bytes: 100,
            disk_write_bytes: 200,
            disk_read_bytes_per_second: 20.0,
            disk_write_bytes_per_second: 40.0,
            open_file_disk_bytes: 4096,
            ..StorageUsage::default()
        },
        energy: EnergyUsage {
            energy_mwh: 2.0,
            battery_percent: 0.004,
            power_watts: 3.6,
            estimated_app_power_watts: 3.6,
            battery_percent_per_hour: 7.2,
            energy_source: "rapl".into(),
            energy_confidence: "low".into(),
            ..EnergyUsage::default()
        },
        ..ResourceUsage::default()
    };
    let now = super::now_milliseconds();
    let bucket = now - now % super::BUCKET_MILLISECONDS;
    for timestamp in [bucket + 2_000, bucket + 7_000, bucket + 12_000] {
        store.record("example.desktop", timestamp, 5.0, &usage);
    }
    store.record(
        "example.desktop",
        bucket + super::BUCKET_MILLISECONDS + 2_000,
        2.0,
        &ResourceUsage::default(),
    );
    let page = store.query("example.desktop", None, None, 10)?;
    assert!(!page.has_more);
    assert_eq!(page.points.len(), 1);
    let point = &page.points[0].resources;
    assert_eq!(point.compute.cpu_percent, 50.0);
    assert_eq!(point.compute.gpu_percent, 25.0);
    assert_eq!(point.compute.gpu_memory_bytes, 2048);
    assert_eq!(point.storage.disk_read_bytes, 300);
    assert_eq!(point.storage.disk_write_bytes, 600);
    assert_eq!(point.storage.disk_read_bytes_per_second, 20.0);
    assert_eq!(point.storage.disk_write_bytes_per_second, 40.0);
    assert_eq!(point.storage.open_file_disk_bytes, 4096);
    assert_eq!(point.energy_mwh, 6.0);
    store.save()?;

    let points = page.points;
    let mut loaded = HistoryStore::load(Some(path));
    assert_eq!(
        loaded.query("example.desktop", None, None, 10)?.points,
        points
    );
    Ok(())
}

#[test]
fn keeps_compact_energy_totals_for_week_overviews() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.json");
    let mut store = HistoryStore::load(Some(path.clone()));
    let usage = ResourceUsage {
        energy: EnergyUsage {
            energy_mwh: 1.25,
            energy_source: "rapl".into(),
            energy_confidence: "low".into(),
            ..EnergyUsage::default()
        },
        ..ResourceUsage::default()
    };
    let current = super::now_milliseconds();
    let now = current - current % super::ENERGY_BUCKET_MILLISECONDS + 1_000;
    store.record("example.desktop", now, 2.0, &usage);
    store.record("example.desktop", now + 2_000, 2.0, &usage);
    let totals = store.energy_totals(now.saturating_sub(1), now + 3_000);
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].energy_mwh, 2.5);
    assert_eq!(totals[0].energy_source, "rapl");
    store.save_final()?;

    let mut loaded = HistoryStore::load(Some(path));
    let totals = loaded.energy_totals(
        now.saturating_sub(1),
        now + super::ENERGY_BUCKET_MILLISECONDS,
    );
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].energy_mwh, 2.5);
    Ok(())
}

#[test]
fn paginates_forward_with_target_bound_cursors() -> anyhow::Result<()> {
    let mut store = HistoryStore::load(None);
    let now = super::now_milliseconds();
    let first_bucket = now.saturating_sub(4 * super::BUCKET_MILLISECONDS)
        / super::BUCKET_MILLISECONDS
        * super::BUCKET_MILLISECONDS;
    for index in 0..3 {
        store.record(
            "example.desktop",
            first_bucket + index * super::BUCKET_MILLISECONDS + 1_000,
            1.0,
            &ResourceUsage::default(),
        );
    }

    let first = store.query("example.desktop", None, None, 2)?;
    assert_eq!(first.points.len(), 2);
    assert!(first.has_more);
    let cursor = first.next_cursor.as_deref().context("next cursor")?;
    let second = store.query("example.desktop", None, Some(cursor), 2)?;
    assert_eq!(second.points.len(), 1);
    assert!(!second.has_more);
    assert!(second.next_cursor.is_some());
    assert!(
        store
            .query("another.desktop", None, Some(cursor), 2)
            .is_err()
    );
    Ok(())
}
