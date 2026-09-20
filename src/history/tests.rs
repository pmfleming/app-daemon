use super::{HistoryStore, persist_snapshot};
use crate::model::{ComputeUsage, EnergyUsage, ResourceUsage, StorageUsage};

#[test]
fn persists_resource_buckets_and_retains_energy_for_seven_days() -> anyhow::Result<()> {
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
    let until = bucket + 2 * super::ENERGY_BUCKET_MILLISECONDS;
    assert_eq!(
        store.energy_totals(bucket, bucket + 17_000)[0].energy_mwh,
        6.0
    );
    persist_snapshot(store.snapshot(true))?;

    let mut loaded = HistoryStore::load(Some(path));
    let restored = loaded.query("example.desktop", None, None, 10)?;
    assert_eq!(
        restored.points.len(),
        2,
        "final save includes the partial bucket"
    );
    assert_eq!(restored.points.first(), page.points.first());
    let totals = loaded.energy_totals(bucket, until);
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].energy_mwh, 6.0);
    assert_eq!(totals[0].energy_source, "rapl");
    for (days, expected_energy) in [(2, 6.0), (8, 0.0)] {
        let timestamp = bucket + days * 24 * 60 * 60 * 1000;
        loaded.record("other.desktop", timestamp, 1.0, &ResourceUsage::default());
        assert!(
            loaded
                .query("example.desktop", None, None, 10)?
                .points
                .is_empty()
        );
        let energy: f64 = loaded
            .energy_totals(bucket, timestamp)
            .iter()
            .map(|total| total.energy_mwh)
            .sum();
        assert_eq!(
            energy, expected_energy,
            "energy retention after {days} days"
        );
    }
    Ok(())
}
