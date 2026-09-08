use super::{
    DiskFile, DiskFileId, MemoryUsage, NetworkCounters, ProcessCache, ProcessFiles, ProcessStat,
    ResourceSampler, test_provider::TestProvider,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

fn set_process(provider: &TestProvider, start_ticks: u64, bytes: u64, socket: u64) {
    let mut state = provider.state.lock().unwrap();
    state.processes.insert(
        42,
        ProcessStat {
            parent_pid: 1,
            total_ticks: 0,
            start_ticks,
            major_faults: 0,
            thread_count: 1,
        },
    );
    state.memory.insert(
        42,
        MemoryUsage {
            rss_bytes: bytes,
            rss_available: true,
            ..Default::default()
        },
    );
    let files = HashMap::from([(
        DiskFileId {
            device: 1,
            inode: start_ticks,
        },
        DiskFile {
            bytes,
            temporary: false,
        },
    )]);
    state.files.insert(
        42,
        ProcessFiles {
            open: files.clone(),
            referenced: files,
            fd_available: true,
        },
    );
    state.sockets.insert(42, HashSet::from([socket]));
    state.network = Some(HashMap::from([(
        socket,
        NetworkCounters {
            received_bytes: bytes,
            transmitted_bytes: 0,
        },
    )]));
}

#[test]
fn cache_refreshes_at_deadline_and_evicts_inactive_identities() {
    let mut cache = ProcessCache::<u64>::default();
    let now = Instant::now();
    let interval = Duration::from_secs(10);
    let mut reads = 0;
    let mut sample = |_| {
        reads += 1;
        reads
    };
    cache.refresh(&HashSet::from([(42, 1)]), now, interval, &mut sample);
    cache.refresh(
        &HashSet::from([(42, 1), (43, 1)]),
        now + interval / 2,
        interval,
        &mut sample,
    );
    assert_eq!(cache.samples[&(42, 1)], 1);
    assert_eq!(cache.samples[&(43, 1)], 2);
    assert_eq!(cache.next_refresh, Some(now + interval));
    cache.refresh(
        &HashSet::from([(42, 1)]),
        now + interval,
        interval,
        &mut sample,
    );
    assert_eq!(cache.samples, HashMap::from([((42, 1), 3)]));
    cache.refresh(&HashSet::new(), now + interval, interval, &mut sample);
    assert!(cache.samples.is_empty());
}

#[test]
fn pid_reuse_invalidates_memory_files_and_socket_baselines_before_ttl() {
    let provider = Arc::new(TestProvider::default());
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let targets = HashMap::from([("app.desktop".into(), vec![42])]);
    set_process(&provider, 1, 100, 10);
    sampler.sample_for_targets(&targets);
    // Ensure this test exercises identity invalidation, not time-based expiry.
    let later = Instant::now() + std::time::Duration::from_secs(3600);
    sampler.memory.next_refresh = Some(later);
    sampler.open_files.next_refresh = Some(later);

    set_process(&provider, 1, 200, 10);
    let cached = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(cached.compute.memory_bytes, 100);
    assert_eq!(cached.storage.open_file_disk_bytes, 100);
    assert_eq!(provider.memory_reads.load(Ordering::Relaxed), 1);
    assert_eq!(provider.file_reads.load(Ordering::Relaxed), 1);

    set_process(&provider, 2, 900, 20);
    let reused = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(reused.compute.memory_bytes, 900);
    assert_eq!(reused.storage.open_file_disk_bytes, 900);
    assert_eq!(reused.network.network_connection_count, 1);
    assert_eq!(
        reused.network.network_receive_bytes, 0,
        "new process gets a baseline"
    );
    assert_eq!(provider.memory_reads.load(Ordering::Relaxed), 2);
    assert_eq!(provider.file_reads.load(Ordering::Relaxed), 2);
    assert!(!sampler.memory.samples.contains_key(&(42, 1)));
    assert!(!sampler.open_files.samples.contains_key(&(42, 1)));
}
