use super::*;
use crate::resources::test_provider::TestProvider;
use std::{sync::{Barrier, atomic::Ordering}, time::Duration};

fn finish(cache: &mut AppDiskCache, provider: &Arc<dyn ResourceProvider>, targets: &[String], now: Instant) -> HashMap<String, DiskBreakdown> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let samples = cache.read(provider, targets, now);
        if cache.in_flight.is_empty() { return samples; }
        assert!(Instant::now() < deadline, "disk worker did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn slow_disk_walks_do_not_block_reads_or_create_unbounded_work() {
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().disk_usage = Some(DiskBreakdown { total_bytes: 100, ..Default::default() });
    let gate = Arc::new(Barrier::new(3));
    *provider.disk_gate.lock().unwrap() = Some(gate.clone());
    let (started, receiver) = mpsc::channel();
    *provider.disk_started.lock().unwrap() = Some(started);
    let shared: Arc<dyn ResourceProvider> = provider.clone();
    let targets = vec!["A".into(), "B".into(), "C".into(), "D".into()];
    let mut cache = AppDiskCache::default();
    let now = Instant::now();
    assert!(cache.read(&shared, &targets, now).is_empty());
    receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 0..10 {
        assert!(cache.read(&shared, &targets, now).is_empty());
        assert_eq!(cache.in_flight.len(), WORKERS);
    }
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 2);
    *provider.disk_gate.lock().unwrap() = None;
    gate.wait();
    let samples = finish(&mut cache, &shared, &targets, now);
    assert_eq!(samples.len(), 4);
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 4);
    assert_eq!(samples["A"].total_bytes, 100);
}

#[test]
fn resource_snapshots_publish_while_disk_worker_is_blocked() {
    use crate::resources::{ProcessStat, ResourceSampler};
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().processes.insert(42, ProcessStat {
        parent_pid: 1, total_ticks: 0, start_ticks: 1, major_faults: 0, thread_count: 1,
    });
    let gate = Arc::new(Barrier::new(2));
    *provider.disk_gate.lock().unwrap() = Some(gate.clone());
    let (started, started_receiver) = mpsc::channel();
    *provider.disk_started.lock().unwrap() = Some(started);
    let (sender, receiver) = mpsc::channel();
    let sampling = std::thread::spawn(move || {
        let mut sampler = ResourceSampler { provider, ..Default::default() };
        let targets = HashMap::from([("A".into(), vec![42])]);
        for _ in 0..2 {
            let usage = sampler.sample_for_targets(&targets).usage_for_target("A", [42]);
            let _ = sender.send(usage);
        }
    });
    started_receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    let first = receiver.recv_timeout(Duration::from_secs(3));
    let second = receiver.recv_timeout(Duration::from_secs(3));
    gate.wait();
    sampling.join().unwrap();
    for usage in [first.unwrap(), second.unwrap()] {
        assert_eq!(usage.compute.process_count, 1);
        assert_eq!(usage.measurement.disk_space_scope, "unavailable");
    }
}

#[test]
fn failed_refresh_keeps_last_completed_footprint_and_respects_ttl() {
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().disk_usage = Some(DiskBreakdown { total_bytes: 100, ..Default::default() });
    let shared: Arc<dyn ResourceProvider> = provider.clone();
    let targets = vec!["A".into()];
    let mut cache = AppDiskCache::default();
    let now = Instant::now();
    assert_eq!(finish(&mut cache, &shared, &targets, now)["A"].total_bytes, 100);
    provider.state.lock().unwrap().disk_usage = None;
    assert_eq!(cache.read(&shared, &targets, now)["A"].total_bytes, 100);
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 1);
    let later = now + APP_DISK_REFRESH_INTERVAL + Duration::from_secs(1);
    assert_eq!(finish(&mut cache, &shared, &targets, later)["A"].total_bytes, 100);
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 2);
    assert!(cache.read(&shared, std::iter::empty::<&String>(), later).is_empty());
}
