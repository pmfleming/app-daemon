use super::test_provider::TestProvider;
use super::{NetworkCounters, ResourceSampler, SampledNetwork};
use std::collections::HashSet;

fn sample(sampler: &mut ResourceSampler, provider: &TestProvider, pids: &[u32]) -> SampledNetwork {
    let result = sampler.sample_network(provider, &pids.iter().copied().collect());
    sampler.previous_network_counters = result.current.clone();
    result
}

fn socket(provider: &TestProvider, pid: u32, inode: u64, received_bytes: u64) {
    let mut state = provider.state.lock().unwrap();
    state.sockets.entry(pid).or_default().insert(inode);
    state.network.get_or_insert_default().insert(
        inode,
        NetworkCounters {
            received_bytes,
            transmitted_bytes: received_bytes / 2,
        },
    );
}

#[test]
fn discovers_new_sockets_and_initial_bytes_each_sample() {
    let provider = TestProvider::default();
    let mut sampler = ResourceSampler::default();
    socket(&provider, 42, 100, 5000);
    let first = sample(&mut sampler, &provider, &[42]);
    assert_eq!(first.deltas[&100].received_bytes, 0, "baseline on startup");

    socket(&provider, 42, 100, 5100);
    socket(&provider, 42, 101, 2000);
    let next = sample(&mut sampler, &provider, &[42]);
    assert_eq!(next.deltas[&100].received_bytes, 100);
    assert_eq!(next.deltas[&101].received_bytes, 2000);
    assert_eq!(next.deltas[&101].transmitted_bytes, 1000);
    assert_eq!(sampler.previous_sockets_by_pid[&42].len(), 2);

    provider
        .state
        .lock()
        .unwrap()
        .sockets
        .get_mut(&42)
        .unwrap()
        .remove(&100);
    let closed = sample(&mut sampler, &provider, &[42]);
    assert_eq!(*sampler.previous_sockets_by_pid[&42], HashSet::from([101]));
    assert!(!closed.deltas.contains_key(&100));
    assert_eq!(
        provider
            .file_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn baselines_newly_attributed_processes_and_counter_recovery() {
    let provider = TestProvider::default();
    let mut sampler = ResourceSampler::default();
    socket(&provider, 42, 100, 5000);
    sample(&mut sampler, &provider, &[42]);
    socket(&provider, 43, 101, 8000);
    assert_eq!(
        sample(&mut sampler, &provider, &[42, 43]).deltas[&101].received_bytes,
        0
    );

    provider.state.lock().unwrap().network = None;
    assert!(!sample(&mut sampler, &provider, &[42, 43]).available);
    socket(&provider, 42, 102, 9000);
    assert_eq!(
        sample(&mut sampler, &provider, &[42, 43]).deltas[&102].received_bytes,
        0
    );
}

#[test]
fn unreadable_descriptors_do_not_reuse_stale_socket_ownership() {
    let provider = TestProvider::default();
    let mut sampler = ResourceSampler::default();
    socket(&provider, 42, 100, 5000);
    sample(&mut sampler, &provider, &[42]);
    provider.state.lock().unwrap().sockets.remove(&42);
    sample(&mut sampler, &provider, &[42]);
    assert!(!sampler.previous_sockets_by_pid.contains_key(&42));
    socket(&provider, 42, 101, 9000);
    assert_eq!(
        sample(&mut sampler, &provider, &[42]).deltas[&101].received_bytes,
        0
    );
}
