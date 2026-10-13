use super::{BUCKET_MILLISECONDS, HistoryStore, now_milliseconds};
use crate::model::{HistoricalResourceUsage, ResourceUsage};

fn usage() -> ResourceUsage {
    let mut usage = ResourceUsage::default();
    usage.measurement.coverage = 1.0;
    usage.compute.cpu_percent_of_machine = 100.0;
    usage.storage.disk_read_bytes = 101;
    usage.storage.cancelled_write_bytes = u64::MAX;
    usage.network.network_receive_bytes = 73;
    usage.network.network_transmit_bytes = 1;
    usage.energy.energy_mwh = 4.0;
    usage.energy.energy_source = "rapl".into();
    usage
}

fn base() -> u64 {
    now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS - 120_000
}

#[test]
fn crossing_sample_splits_totals_and_keeps_partial_bucket_endpoint() {
    let base = base();
    let mut store = HistoryStore::load(None);
    store.record("app", base + 25_000, 20.0, &usage());
    let page = store.query("app", None, None, 10).unwrap();
    assert_eq!(
        page.points
            .iter()
            .map(|p| (p.timestamp_ms - base, p.duration_ms))
            .collect::<Vec<_>>(),
        [(15_000, 10_000), (25_000, 10_000)]
    );
    let total = |read: fn(&HistoricalResourceUsage) -> u64| {
        page.points.iter().map(|p| read(&p.resources)).sum::<u64>()
    };
    assert_eq!(total(|r| r.storage.disk_read_bytes), 101);
    assert_eq!(total(|r| r.network.network_receive_bytes), 73);
    assert_eq!(total(|r| r.storage.cancelled_write_bytes), u64::MAX);
    assert_eq!(total(|r| r.network.network_transmit_bytes), 1);
    assert_eq!(
        page.points
            .iter()
            .map(|p| p.resources.energy_mwh)
            .sum::<f64>(),
        4.0
    );
    assert!(
        page.points
            .iter()
            .all(|p| p.resources.compute.cpu_percent_of_machine == 100.0)
    );
    for (start, end, observed_ms) in [
        (0, 5_000, 0),
        (5_000, 25_000, 20_000),
        (12_000, 18_000, 6_000),
        (25_000, 30_000, 0),
    ] {
        let page = store
            .query_window("app", Some(base + start), Some(base + end), None, 10)
            .unwrap();
        assert_eq!(
            page.summary.metrics["cpu_percent_of_machine"].observed_ms,
            observed_ms
        );
    }
}
