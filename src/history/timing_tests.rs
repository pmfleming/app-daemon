use super::{BUCKET_MILLISECONDS, HistoryStore, now_milliseconds, persist_snapshot};
use crate::model::ResourceUsage;

fn usage() -> ResourceUsage {
    let mut usage = ResourceUsage::default();
    usage.measurement.coverage = 1.0;
    usage.compute.cpu_percent_of_machine = 100.0;
    usage.storage.disk_read_bytes = 101;
    usage.network.network_receive_bytes = 73;
    usage.energy.energy_mwh = 4.0;
    usage.energy.energy_source = "rapl".into();
    usage
}

fn base() -> u64 {
    now_milliseconds() / BUCKET_MILLISECONDS * BUCKET_MILLISECONDS - 120_000
}

#[test]
fn boundary_endpoint_does_not_shift_observations_into_the_next_bucket() {
    let base = base();
    let mut store = HistoryStore::load(None);
    store.record("app", base + 15_000, 10.0, &usage());
    let observed = store
        .query_window("app", Some(base + 5_000), Some(base + 15_000), None, 10)
        .unwrap();
    assert_eq!(observed.points[0].timestamp_ms, base + 15_000);
    assert_eq!(observed.points[0].duration_ms, 10_000);
    let cpu = &observed.summary.metrics["cpu_percent_of_machine"];
    assert_eq!(cpu.observed_ms, 10_000);
    assert_eq!(cpu.coverage, 1.0);
    let unobserved = store
        .query_window("app", Some(base + 20_000), Some(base + 30_000), None, 10)
        .unwrap();
    assert_eq!(
        unobserved.summary.metrics["cpu_percent_of_machine"].observed_ms,
        0
    );
    assert_eq!(
        unobserved.summary.metrics["cpu_percent_of_machine"].mean,
        None
    );
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
    assert_eq!(
        page.points
            .iter()
            .map(|p| p.resources.storage.disk_read_bytes)
            .sum::<u64>(),
        101
    );
    assert_eq!(
        page.points
            .iter()
            .map(|p| p.resources.network.network_receive_bytes)
            .sum::<u64>(),
        73
    );
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
        let page = store
            .query_window("app", Some(base + start), Some(base + end), None, 10)
            .unwrap();
        assert_eq!(
            page.summary.metrics["cpu_percent_of_machine"].observed_ms,
            observed_ms
        );
    }
}

#[test]
fn gaps_within_a_bucket_survive_restart_without_fabricated_coverage() {
    let base = base();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.json");
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", base + 3_000, 2.0, &usage());
    persist_snapshot(store.snapshot(true)).unwrap();
    let mut store = HistoryStore::load(Some(path.clone()));
    store.record("app", base + 9_000, 2.0, &usage());
    persist_snapshot(store.snapshot(true)).unwrap();
    let mut store = HistoryStore::load(Some(path));
    let page = store.query("app", None, None, 10).unwrap();
    assert_eq!(page.points.len(), 2);
    let gap = store
        .query_window("app", Some(base + 3_000), Some(base + 7_000), None, 10)
        .unwrap();
    assert_eq!(gap.summary.metrics["cpu_percent_of_machine"].observed_ms, 0);
}
