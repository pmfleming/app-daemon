use super::super::{ResourceSampler, provider::ProcessStat, test_provider::TestProvider};
use super::{
    INET_DIAG_INFO, INET_DIAG_MESSAGE_LENGTH, TCP_INFO_BYTES_ACKED_OFFSET,
    TCP_INFO_BYTES_RECEIVED_OFFSET, TCP_INFO_COUNTERS_LENGTH, parse_diag_message, write_u16,
    write_u32,
};
use crate::model::ResourceUsage;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

fn sample(sampler: &mut ResourceSampler, pids: &[u32]) -> ResourceUsage {
    sampler
        .sample_for_targets(&HashMap::from([("app.desktop".into(), pids.to_vec())]))
        .usage_for_roots(pids.iter().copied())
}

fn socket(provider: &TestProvider, pid: u32, inode: u64, received_bytes: u64) {
    let mut state = provider.state.lock().unwrap();
    state.processes.insert(
        pid,
        ProcessStat {
            parent_pid: 1,
            total_ticks: 0,
            start_ticks: 1,
            major_faults: 0,
            thread_count: 1,
        },
    );
    state.sockets.entry(pid).or_default().insert(inode);
    // Feed the sampler decoded kernel messages rather than pre-decoded counters.
    let mut message = vec![0_u8; INET_DIAG_MESSAGE_LENGTH + 4 + TCP_INFO_COUNTERS_LENGTH];
    write_u32(&mut message, 68, inode as u32);
    write_u16(
        &mut message,
        INET_DIAG_MESSAGE_LENGTH,
        (4 + TCP_INFO_COUNTERS_LENGTH) as u16,
    );
    write_u16(&mut message, INET_DIAG_MESSAGE_LENGTH + 2, INET_DIAG_INFO);
    let payload = INET_DIAG_MESSAGE_LENGTH + 4;
    for (offset, bytes) in [
        (TCP_INFO_BYTES_ACKED_OFFSET, received_bytes / 2),
        (TCP_INFO_BYTES_RECEIVED_OFFSET, received_bytes),
    ] {
        message[payload + offset..payload + offset + 8].copy_from_slice(&bytes.to_ne_bytes());
    }
    parse_diag_message(
        &message,
        &HashSet::from([inode]),
        state.network.get_or_insert_default(),
    );
}

#[test]
fn socket_lifecycle_attributes_deltas_and_rebaselines_new_or_unavailable_processes() {
    let provider = Arc::new(TestProvider::default());
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    socket(&provider, 42, 100, 5000);
    assert_eq!(sample(&mut sampler, &[42]).network.network_receive_bytes, 0);

    socket(&provider, 42, 100, 5100);
    socket(&provider, 42, 101, 2000);
    let next = sample(&mut sampler, &[42]).network;
    assert_eq!(next.network_receive_bytes, 2100);
    assert_eq!(next.network_transmit_bytes, 1050);
    assert_eq!(next.network_connection_count, 2);

    socket(&provider, 42, 100, 9000);
    provider
        .state
        .lock()
        .unwrap()
        .sockets
        .get_mut(&42)
        .unwrap()
        .remove(&100);
    let closed = sample(&mut sampler, &[42]).network;
    assert_eq!(closed.network_connection_count, 1);
    assert_eq!(
        closed.network_receive_bytes, 0,
        "unowned sockets must not contribute traffic"
    );

    // A newly attributed process starts with a baseline, unlike a new socket
    // belonging to an already observed process.
    socket(&provider, 43, 104, 8000);
    assert_eq!(
        sample(&mut sampler, &[42, 43])
            .network
            .network_receive_bytes,
        0
    );

    provider.state.lock().unwrap().network = None;
    assert!(
        !sample(&mut sampler, &[42, 43])
            .measurement
            .network_bytes_available
    );
    socket(&provider, 42, 102, 9000);
    assert_eq!(
        sample(&mut sampler, &[42, 43])
            .network
            .network_receive_bytes,
        0
    );
    socket(&provider, 42, 102, 9100);
    assert_eq!(
        sample(&mut sampler, &[42, 43])
            .network
            .network_receive_bytes,
        100
    );

    provider.state.lock().unwrap().sockets.remove(&42);
    let missing = sample(&mut sampler, &[42]);
    assert_eq!(missing.network.network_connection_count, 0);
    assert!(!missing.measurement.network_connections_available);
    socket(&provider, 42, 103, 9000);
    assert_eq!(sample(&mut sampler, &[42]).network.network_receive_bytes, 0);
}
