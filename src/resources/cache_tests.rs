use super::{
    ResourceSampler,
    provider::{
        DiskFile, DiskFileId, GpuProcessStat, MemoryUsage, NetworkCounters, ProcessFiles,
        ProcessStat,
    },
    test_provider::TestProvider,
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
    state.gpu.insert(
        42,
        GpuProcessStat {
            engine_nanoseconds: HashMap::from([("device/client/gfx".into(), bytes * 1_000_000)]),
            resident_memory_bytes: bytes,
            ..Default::default()
        },
    );
    state.io.entry(42).or_default().storage.read_bytes = bytes;
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
            open: files,
            mapped: HashMap::new(),
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
fn identity_changes_resume_and_exit_rebaseline_resource_caches() {
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
    assert!(current.measurement.gpu_available);
    assert_eq!(current.compute.gpu_memory_bytes, 200);
    assert_eq!(current.storage.disk_read_bytes, 100);
    assert_eq!(current.network.network_receive_bytes, 100);
    assert!(current.compute.major_faults_per_second > 0.0);

    // A new socket in a continuously observed process includes its initial bytes.
    set_process(&provider, 1, 250, 11);
    let opened = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(opened.network.network_receive_bytes, 250);
    provider.state.lock().unwrap().network = None;
    let missing = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert!(!missing.measurement.network_bytes_available);
    set_process(&provider, 1, 300, 11);
    let recovered = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(recovered.network.network_receive_bytes, 0);

    // Reusing both the PID and inode must not carry the old process's counters.
    set_process(&provider, 2, 900, 11);
    let reused = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(reused.compute.memory_bytes, 900);
    assert_eq!(reused.storage.open_file_disk_bytes, 900);
    assert_eq!(reused.network.network_connection_count, 1);
    assert_eq!(reused.compute.cpu_percent, 0.0);
    assert_eq!(reused.compute.gpu_percent, 0.0);
    assert_eq!(reused.storage.disk_read_bytes, 0);
    assert_eq!(reused.compute.major_faults_per_second, 0.0);
    assert_eq!(reused.network.network_receive_bytes, 0);

    // Resume is another discontinuity: the lower energy counter is not a wrap,
    // and pre-suspend CPU, I/O and socket work must not become a history interval.
    let zone = std::path::PathBuf::from("package-0");
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(zone.clone(), (900_000_000, 1_000_000_000));
    sampler.sample_for_targets(&targets);
    sampler.reset_after_resume();
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(zone.clone(), (100, 1_000_000_000));
    set_process(&provider, 2, 950, 20);
    let resumed = sampler.sample_for_targets(&targets);
    assert_eq!(resumed.interval_seconds(), 0.0);
    assert_eq!(resumed.system_energy_mwh, 0.0);
    assert_eq!(resumed.energy_source, "unavailable");
    let usage = resumed.usage_for_roots([42]);
    assert_eq!(usage.compute.cpu_percent, 0.0);
    assert_eq!(usage.compute.gpu_percent, 0.0);
    assert_eq!(usage.storage.disk_read_bytes, 0);
    assert_eq!(usage.network.network_receive_bytes, 0);
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(zone, (7_200_100, 1_000_000_000));
    let next = sampler.sample_for_targets(&targets);
    assert!(next.interval_seconds() > 0.0);
    assert_eq!(
        next.system_energy_mwh, 2.0,
        "only post-resume work is counted"
    );
    assert_eq!(next.energy_source, "rapl");

    provider.state.lock().unwrap().processes.clear();
    sampler.sample_for_targets(&targets);
    set_process(&provider, 2, 1000, 20);
    let returned = sampler.sample_for_targets(&targets).usage_for_roots([42]);
    assert_eq!(returned.storage.disk_read_bytes, 0);
    assert_eq!(returned.compute.cpu_percent, 0.0);
    assert_eq!(returned.compute.gpu_percent, 0.0);
    assert_eq!(returned.network.network_receive_bytes, 0);
    let inactive = sampler
        .sample_for_targets(&HashMap::new())
        .usage_for_roots([42]);
    assert!(!inactive.measurement.referenced_files_available);
    assert!(!inactive.measurement.gpu_available);
    assert_eq!(inactive.storage.open_file_disk_bytes, 0);
}
