use super::{HistoryStore, persist_snapshot};
use crate::model::{
    ComputeUsage, EnergyUsage, HistoricalResourceUsage, NetworkUsage, ResourceMeasurement,
    ResourceUsage, StorageUsage,
};

#[test]
fn split_buckets_persist_without_losing_totals_availability_or_energy_retention()
-> anyhow::Result<()> {
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
            ..Default::default()
        },
        storage: StorageUsage {
            disk_read_bytes: 101,
            disk_write_bytes: 200,
            logical_read_bytes: 51,
            read_operations: 13,
            cancelled_write_bytes: u64::MAX,
            open_file_disk_bytes: 4096,
            ..Default::default()
        },
        network: NetworkUsage {
            network_receive_bytes: 71,
            network_transmit_bytes: 1,
            ..Default::default()
        },
        energy: EnergyUsage {
            energy_mwh: 4.0,
            battery_percent: 0.004,
            power_watts: 3.6,
            estimated_app_power_watts: 3.6,
            battery_percent_per_hour: 7.2,
            energy_source: "rapl".into(),
            energy_confidence: "low".into(),
            ..Default::default()
        },
        measurement: ResourceMeasurement {
            coverage: 1.0,
            memory_source: "pss".into(),
            gpu_available: true,
            storage_available: true,
            network_connections_available: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let now = super::now_milliseconds();
    let bucket = now - now % super::BUCKET_MILLISECONDS;
    store.record("example.desktop", bucket + 25_000, 20.0, &usage);
    // Final saving must include the partial second bucket, with its actual endpoint.
    persist_snapshot(store.snapshot(true).unwrap())?;
    let page = store.query_window("example.desktop", None, None, None, 10)?;
    assert!(!page.has_more);
    assert_eq!(
        page.points
            .iter()
            .map(|p| (p.timestamp_ms - bucket, p.duration_ms))
            .collect::<Vec<_>>(),
        [(15_000, 10_000), (25_000, 10_000)]
    );
    let total = |read: fn(&HistoricalResourceUsage) -> u64| {
        page.points.iter().map(|p| read(&p.resources)).sum::<u64>()
    };
    assert_eq!(total(|r| r.storage.disk_read_bytes), 101);
    assert_eq!(total(|r| r.storage.cancelled_write_bytes), u64::MAX);
    assert_eq!(total(|r| r.network.network_receive_bytes), 71);
    assert_eq!(total(|r| r.network.network_transmit_bytes), 1);
    let point = &page.points[0].resources;
    assert_eq!(
        page.summary.metrics["gpu_busy_percent"].mean,
        Some(0.0),
        "idle is available"
    );
    assert!(point.metric_availability.0["network_connection_count"]);
    assert!(!point.metric_availability.0["network_receive_bytes_per_second"]);
    assert_eq!(point.compute.cpu_percent, 50.0);
    assert_eq!(point.compute.gpu_percent, 25.0);
    assert_eq!(point.compute.gpu_memory_bytes, 2048);
    assert_eq!(point.storage.disk_read_bytes_per_second, 5.0);
    assert_eq!(point.storage.disk_write_bytes_per_second, 10.0);
    assert_eq!(point.storage.open_file_disk_bytes, 4096);
    assert_eq!(point.storage.logical_read_bytes_per_second, 2.5);
    assert_eq!(point.storage.read_operations_per_second, 0.6);
    assert_eq!(point.network.network_receive_bytes_per_second, 3.5);
    assert_eq!(point.network.network_transmit_bytes_per_second, 0.0);
    assert_eq!(
        page.points
            .iter()
            .map(|p| p.resources.energy_mwh)
            .sum::<f64>(),
        4.0
    );
    for (start, end, observed_ms) in [
        (0, 5_000, 0),
        (5_000, 25_000, 20_000),
        (12_000, 18_000, 6_000),
        (25_000, 30_000, 0),
    ] {
        let page = store.query_window(
            "example.desktop",
            Some(bucket + start),
            Some(bucket + end),
            None,
            10,
        )?;
        assert_eq!(
            page.summary.metrics["cpu_percent_of_machine"].observed_ms,
            observed_ms
        );
    }
    let until = bucket + 2 * super::ENERGY_BUCKET_MILLISECONDS;
    assert_eq!(store.energy_totals(bucket, until)[0].energy_mwh, 4.0);
    // The rewritten partial bucket mixes unavailable and measured intervals.
    store.record(
        "example.desktop",
        bucket + 27_000,
        2.0,
        &ResourceUsage::default(),
    );
    store.record("example.desktop", bucket + 29_000, 2.0, &usage);
    persist_snapshot(store.snapshot(true).unwrap())?;
    let mut loaded = HistoryStore::load(Some(path));
    let restored = loaded.query_window("example.desktop", None, None, None, 10)?;
    assert_eq!(restored.points.len(), 2);
    assert_eq!(restored.points.first(), page.points.first());
    let available = restored.points[1].resources.availability.unwrap();
    assert!(!available.gpu && !available.memory && !available.network_connections);
    let totals = loaded.energy_totals(bucket, until);
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].energy_mwh, 8.0);
    assert_eq!(totals[0].energy_source, "rapl");
    for (days, expected_energy) in [(2, 8.0), (8, 0.0)] {
        let timestamp = bucket + days * 24 * 60 * 60 * 1000;
        loaded.record("other.desktop", timestamp, 1.0, &ResourceUsage::default());
        assert!(
            loaded
                .query_window("example.desktop", None, None, None, 10)?
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
