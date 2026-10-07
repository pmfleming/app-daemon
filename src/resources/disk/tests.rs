use super::{APP_DISK_REFRESH_INTERVAL, AppDiskCache, DiskResult, DiskWorkers, WORKERS};
use crate::resources::{
    ResourceSampler,
    provider::{DiskBreakdown, ProcessStat, ResourceProvider},
    test_provider::TestProvider,
};
use std::{
    collections::HashMap,
    sync::{Arc, Barrier, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

fn channels() -> (
    AppDiskCache,
    mpsc::Receiver<String>,
    mpsc::SyncSender<DiskResult>,
) {
    let (sender, requests) = mpsc::sync_channel(WORKERS);
    let (results, receiver) = mpsc::sync_channel(WORKERS);
    let cache = AppDiskCache {
        workers: Some(DiskWorkers {
            requests: sender,
            results: receiver,
        }),
        ..Default::default()
    };
    (cache, requests, results)
}

#[test]
fn completion_releases_capacity_without_reviving_removed_targets_or_retrying_early() {
    let provider: Arc<dyn ResourceProvider> = Arc::new(TestProvider::default());
    let (mut cache, requests, results) = channels();
    let targets = ["A".to_owned(), "B".to_owned(), "C".to_owned()];
    let now = Instant::now();
    assert!(cache.read(&provider, &targets, now).is_empty());
    assert_eq!(requests.try_iter().collect::<Vec<_>>(), ["A", "B"]);
    cache.read(&provider, &targets, now);
    assert!(
        requests.try_recv().is_err(),
        "running work still occupies capacity"
    );
    for target in ["A", "B"] {
        results
            .send(DiskResult {
                target: target.into(),
                usage: Some(DiskBreakdown {
                    total_bytes: 100,
                    ..Default::default()
                }),
            })
            .unwrap();
    }
    let remaining = [&targets[0], &targets[2]];
    let samples = cache.read(&provider, remaining, now);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples["A"].total_bytes, 100);
    assert!(!cache.samples.contains_key("B"));
    assert_eq!(requests.try_recv().unwrap(), "C");
    results
        .send(DiskResult {
            target: "C".into(),
            usage: None,
        })
        .unwrap();
    assert_eq!(cache.read(&provider, remaining, now).len(), 1);
    assert!(cache.in_flight.is_empty());
    assert!(
        requests.try_recv().is_err(),
        "failed initial reads respect TTL too"
    );
    let later = now + APP_DISK_REFRESH_INTERVAL;
    cache.read(&provider, remaining, later);
    assert_eq!(requests.try_iter().collect::<Vec<_>>(), ["A", "C"]);
    for target in ["A", "C"] {
        results
            .send(DiskResult {
                target: target.into(),
                usage: None,
            })
            .unwrap();
    }
    assert!(cache.read(&provider, [], later).is_empty());
    assert!(cache.samples.is_empty() && cache.in_flight.is_empty());
}

#[test]
fn rejected_sends_never_reserve_in_flight_capacity() {
    let provider: Arc<dyn ResourceProvider> = Arc::new(TestProvider::default());
    let (mut cache, requests, _results) = channels();
    for _ in 0..WORKERS {
        cache
            .workers
            .as_ref()
            .unwrap()
            .requests
            .try_send("occupied".into())
            .unwrap();
    }
    let targets = ["A".to_owned()];
    cache.read(&provider, &targets, Instant::now());
    assert!(cache.in_flight.is_empty(), "full channel rejected the send");
    drop(requests);
    cache.read(&provider, &targets, Instant::now());
    assert!(
        cache.in_flight.is_empty(),
        "disconnected channel rejected the send"
    );
}

fn finish(
    cache: &mut AppDiskCache,
    provider: &Arc<dyn ResourceProvider>,
    targets: &[String],
    now: Instant,
) -> HashMap<String, DiskBreakdown> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let samples = cache.read(provider, targets, now);
        if cache.in_flight.is_empty() {
            return samples;
        }
        assert!(Instant::now() < deadline, "disk worker did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn blocked_disk_work_is_bounded_without_stalling_resource_publication() {
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().processes.insert(
        42,
        ProcessStat {
            parent_pid: 1,
            total_ticks: 0,
            start_ticks: 1,
            major_faults: 0,
            thread_count: 1,
        },
    );
    let gate = Arc::new(Barrier::new(WORKERS + 1));
    *provider.disk_gate.lock().unwrap() = Some(gate.clone());
    let (started, started_receiver) = mpsc::channel();
    *provider.disk_started.lock().unwrap() = Some(started);
    let (sender, receiver) = mpsc::channel();
    let shared = provider.clone();
    let sampling = std::thread::spawn(move || {
        let mut sampler = ResourceSampler {
            provider: shared,
            ..Default::default()
        };
        let targets = (0..WORKERS + 2)
            .map(|index| (format!("{index}.desktop"), vec![42]))
            .collect();
        for _ in 0..3 {
            let usage = sampler
                .sample_for_targets(&targets)
                .usage_for_target("0.desktop", [42]);
            let _ = sender.send(usage);
        }
    });
    for _ in 0..WORKERS {
        started_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
    }
    let samples: Vec<_> = (0..3)
        .map(|_| receiver.recv_timeout(Duration::from_secs(3)))
        .collect();
    let started_reads = provider.disk_reads.load(Ordering::Relaxed);
    // Release workers before asserting results, including when sampling timed out.
    *provider.disk_gate.lock().unwrap() = None;
    gate.wait();
    sampling.join().unwrap();
    assert_eq!(
        started_reads, WORKERS as u64,
        "outstanding scans must stay bounded"
    );
    for usage in samples {
        let usage = usage.unwrap();
        assert_eq!(usage.compute.process_count, 1);
        assert_eq!(usage.measurement.disk_space_scope, "unavailable");
    }
}

#[test]
fn failed_refresh_keeps_last_completed_footprint_and_respects_ttl() {
    let provider = Arc::new(TestProvider::default());
    provider.state.lock().unwrap().disk_usage = Some(DiskBreakdown {
        total_bytes: 100,
        ..Default::default()
    });
    let shared: Arc<dyn ResourceProvider> = provider.clone();
    let targets = vec!["A".into()];
    let mut cache = AppDiskCache::default();
    let now = Instant::now();
    assert_eq!(
        finish(&mut cache, &shared, &targets, now)["A"].total_bytes,
        100
    );
    provider.state.lock().unwrap().disk_usage = None;
    assert_eq!(cache.read(&shared, &targets, now)["A"].total_bytes, 100);
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 1);
    let later = now + APP_DISK_REFRESH_INTERVAL + Duration::from_secs(1);
    assert_eq!(
        finish(&mut cache, &shared, &targets, later)["A"].total_bytes,
        100
    );
    assert_eq!(provider.disk_reads.load(Ordering::Relaxed), 2);
    assert!(
        cache
            .read(&shared, std::iter::empty::<&String>(), later)
            .is_empty()
    );
}
