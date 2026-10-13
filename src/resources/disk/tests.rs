use super::{APP_DISK_REFRESH_INTERVAL, AppDiskCache, DiskResult, DiskWorkers, WORKERS};
use crate::resources::{
    ResourceSampler,
    provider::{DiskBreakdown, ProcessStat, ResourceProvider},
    test_provider::TestProvider,
};
use std::{
    sync::{Arc, Barrier, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

#[test]
fn disk_cache_retains_completed_sizes_retries_failures_and_discards_removed_targets() {
    let provider: Arc<dyn ResourceProvider> = Arc::new(TestProvider::default());
    let (sender, requests) = mpsc::sync_channel(WORKERS);
    let (results, receiver) = mpsc::sync_channel(WORKERS);
    let mut cache = AppDiskCache {
        workers: Some(DiskWorkers {
            requests: sender,
            results: receiver,
        }),
        ..Default::default()
    };
    let targets = ["A".to_owned(), "B".to_owned(), "C".to_owned()];
    let now = Instant::now();
    assert!(cache.read(&provider, &targets, now).is_empty());
    for target in requests.try_iter() {
        results
            .send(DiskResult {
                target,
                usage: Some(DiskBreakdown {
                    total_bytes: 100,
                    ..Default::default()
                }),
            })
            .unwrap();
    }
    // B disappears while its work is outstanding. C must get the freed slot.
    let remaining = [&targets[0], &targets[2]];
    let samples = cache.read(&provider, remaining, now);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples["A"].total_bytes, 100);
    assert_eq!(requests.try_recv().unwrap(), "C");
    results
        .send(DiskResult {
            target: "C".into(),
            usage: None,
        })
        .unwrap();
    assert_eq!(cache.read(&provider, remaining, now).len(), 1);
    assert!(
        requests.try_recv().is_err(),
        "failed initial reads must back off"
    );

    let later = now + APP_DISK_REFRESH_INTERVAL;
    cache.read(&provider, remaining, later);
    let retried = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(retried.len(), 2);
    for target in retried {
        results
            .send(DiskResult {
                target,
                usage: None,
            })
            .unwrap();
    }
    assert_eq!(
        cache.read(&provider, remaining, later)["A"].total_bytes,
        100,
        "failed refresh must retain the last completed footprint"
    );
    assert!(
        requests.try_recv().is_err(),
        "failed refreshes must back off too"
    );
    assert!(cache.read(&provider, [], later).is_empty());
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
