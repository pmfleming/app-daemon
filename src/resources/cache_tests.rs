use super::{
    DiskFile, DiskFileId, MemoryUsage, NetworkCounters, ProcessFiles, ProcessIo, ProcessStat,
    ResourceSampler, test_provider::TestProvider,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

fn set_process(provider: &TestProvider, start_ticks: u64, bytes: u64, socket: u64) {
    let mut state = provider.state.lock().unwrap();
    state.system_ticks += 100;
    state.processes.insert(
        42,
        ProcessStat {
            parent_pid: 1,
            total_ticks: bytes,
            start_ticks,
            major_faults: bytes,
            thread_count: 1,
        },
    );
    state.io.insert(
        42,
        ProcessIo {
            physical_read_bytes: bytes,
            ..Default::default()
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
fn pid_reuse_refreshes_resources_without_charging_previous_process_work() {
    let provider = Arc::new(TestProvider::default());
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let targets = HashMap::from([("app.desktop".into(), vec![42])]);
    set_process(&provider, 1, 100, 10);
    sampler.sample_for_targets(&targets);

    set_process(&provider, 1, 200, 10);
    let current = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(current.compute.cpu_percent, 100.0);
    assert_eq!(current.storage.disk_read_bytes, 100);
    assert_eq!(current.network.network_receive_bytes, 100);
    assert!(current.compute.major_faults_per_second > 0.0);

    set_process(&provider, 2, 900, 20);
    let reused = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(reused.compute.memory_bytes, 900);
    assert_eq!(reused.storage.open_file_disk_bytes, 900);
    assert_eq!(reused.network.network_connection_count, 1);
    assert_eq!(reused.compute.cpu_percent, 0.0);
    assert_eq!(reused.storage.disk_read_bytes, 0);
    assert_eq!(reused.compute.major_faults_per_second, 0.0);
    assert_eq!(reused.network.network_receive_bytes, 0);
}
