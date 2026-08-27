use super::{
    BatterySample, CgroupCounters, DiskBreakdown, DiskFile, DiskFileId, EnergyProvider,
    GpuProcessStat, MemoryUsage, NetworkCounters, ProcessFiles, ProcessIo, ProcessStat,
    ProcessUsage, ResourceProvider, ResourceSampler, ResourceSnapshot, equals_key_values,
    parse_process_stat, whitespace_key_values,
};
use anyhow::Context;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Debug)]
struct FakeProvider;

impl EnergyProvider for FakeProvider {
    fn rapl_zones(&self) -> HashMap<PathBuf, (u64, u64)> {
        HashMap::new()
    }

    fn batteries(&self) -> BatterySample {
        BatterySample::default()
    }
}

impl ResourceProvider for FakeProvider {
    fn system_cpu(&self) -> (u64, usize) {
        (100, 8)
    }

    fn processes(&self) -> HashMap<u32, ProcessStat> {
        HashMap::from([(
            42,
            ProcessStat {
                parent_pid: 1,
                total_ticks: 10,
                start_ticks: 5,
                major_faults: 0,
                thread_count: 2,
            },
        )])
    }

    fn process_memory(&self, _pid: u32) -> MemoryUsage {
        MemoryUsage {
            rss_bytes: 2_048,
            pss_bytes: 1_024,
            rss_available: true,
            pss_available: true,
            ..MemoryUsage::default()
        }
    }

    fn process_io(&self, _pid: u32) -> Option<ProcessIo> {
        Some(ProcessIo::default())
    }

    fn process_files(&self, _pid: u32) -> ProcessFiles {
        ProcessFiles {
            fd_available: true,
            ..ProcessFiles::default()
        }
    }

    fn network_counters(&self, _inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>> {
        Some(HashMap::new())
    }

    fn gpu_processes(&self, _pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
        HashMap::new()
    }

    fn process_cgroup(&self, _pid: u32) -> Option<String> {
        None
    }

    fn cgroup_counters(&self, _path: &str) -> Option<CgroupCounters> {
        None
    }

    fn cgroup_members(&self, _path: &str) -> HashSet<u32> {
        HashSet::new()
    }

    fn application_disk_usage(&self, _target_id: &str) -> DiskBreakdown {
        DiskBreakdown::default()
    }
}

#[test]
fn samples_through_an_injected_provider() {
    let mut sampler = ResourceSampler::with_provider(Arc::new(FakeProvider));
    let targets = HashMap::from([("example.desktop".into(), vec![42])]);
    let snapshot = sampler.sample_for_targets(&targets);
    let usage = snapshot.usage_for_target("example.desktop", [42]);
    assert_eq!(snapshot.logical_cpus, 8);
    assert_eq!(usage.compute.memory_bytes, 1_024);
    assert_eq!(usage.compute.thread_count, 2);
    assert_eq!(usage.measurement.memory_source, "pss");
    assert!(usage.measurement.network_connections_available);
}

#[test]
fn parses_proc_stat_with_spaces_in_command() -> anyhow::Result<()> {
    let stat = "42 (application helper) S 7 0 0 0 0 0 0 0 0 0 120 30 0 0 0 0 0 0 99 0 0";
    let process = parse_process_stat(stat).context("valid stat")?;
    assert_eq!(process.parent_pid, 7);
    assert_eq!(process.total_ticks, 150);
    assert_eq!(process.start_ticks, 99);
    assert_eq!(process.major_faults, 0);
    Ok(())
}

#[test]
fn parses_cgroup_cpu_and_io_counters() {
    let cpu = whitespace_key_values("usage_usec 125000\nuser_usec 100000\n");
    assert_eq!(cpu["usage_usec"], 125_000);
    let io = equals_key_values("8:0 rbytes=4096 wbytes=8192 rios=3 wios=4");
    assert_eq!(io["rbytes"], 4096);
    assert_eq!(io["wios"], 4);
}

#[test]
fn totals_process_trees_without_double_counting_shared_roots() {
    let file = |inode, bytes| {
        (
            DiskFileId { device: 1, inode },
            DiskFile {
                bytes,
                temporary: false,
            },
        )
    };
    let process = |parent_pid,
                   cpu_percent,
                   memory_bytes,
                   disk_read_bytes,
                   disk_write_bytes,
                   open_files: HashMap<_, _>| ProcessUsage {
        parent_pid,
        cpu_percent,
        memory: MemoryUsage {
            rss_bytes: memory_bytes,
            ..MemoryUsage::default()
        },
        io: ProcessIo {
            physical_read_bytes: disk_read_bytes,
            physical_write_bytes: disk_write_bytes,
            ..ProcessIo::default()
        },
        files: Arc::new(ProcessFiles {
            referenced: open_files.clone(),
            open: open_files,
            fd_available: true,
            ..ProcessFiles::default()
        }),
        storage_available: true,
        ..ProcessUsage::default()
    };
    let processes = HashMap::from([
        (
            10,
            process(
                1,
                2.0,
                100,
                10,
                20,
                HashMap::from([file(1, 1024), file(2, 2048)]),
            ),
        ),
        (
            11,
            process(10, 3.5, 200, 30, 40, HashMap::from([file(1, 1024)])),
        ),
        (20, process(1, 1.0, 50, 50, 60, HashMap::new())),
    ]);
    let children = HashMap::from([(1, vec![10, 20]), (10, vec![11])]);
    let snapshot = ResourceSnapshot {
        processes,
        children,
        logical_cpus: 4,
        total_process_cpu_percent: 10.0,
        interval_seconds: 2.0,
        system_energy_mwh: 5.0,
        battery_full_mwh: 50_000.0,
        energy_source: "rapl".into(),
        ..ResourceSnapshot::default()
    };
    let usage = snapshot.usage_for_roots([10, 10, 11]);
    assert_eq!(usage.compute.cpu_percent, 5.5);
    assert_eq!(usage.compute.cpu_percent_of_machine, 1.4);
    assert_eq!(usage.compute.memory_bytes, 300);
    assert_eq!(usage.storage.disk_read_bytes, 40);
    assert_eq!(usage.storage.disk_write_bytes, 60);
    assert_eq!(usage.storage.disk_read_bytes_per_second, 20.0);
    assert_eq!(usage.storage.disk_write_bytes_per_second, 30.0);
    assert_eq!(usage.storage.open_file_disk_bytes, 3072);
    assert_eq!(usage.storage.referenced_file_permanent_bytes, 3072);
    assert_eq!(usage.energy.energy_mwh, 2.75);
    assert_eq!(usage.energy.energy_source, "rapl");
}

#[test]
fn aggregates_network_deltas_for_known_application_sockets() {
    let socket_inode = 77;
    let process = ProcessUsage {
        parent_pid: 1,
        memory: MemoryUsage {
            rss_available: true,
            ..MemoryUsage::default()
        },
        files: Arc::new(ProcessFiles {
            sockets: HashSet::from([socket_inode]),
            fd_available: true,
            ..ProcessFiles::default()
        }),
        ..ProcessUsage::default()
    };
    let snapshot = ResourceSnapshot {
        processes: HashMap::from([(42, process)]),
        children: HashMap::from([(1, vec![42])]),
        network_deltas: HashMap::from([(
            socket_inode,
            NetworkCounters {
                received_bytes: 4_096,
                transmitted_bytes: 2_048,
            },
        )]),
        network_counters_available: true,
        interval_seconds: 2.0,
        ..ResourceSnapshot::default()
    };
    let usage = snapshot.usage_for_roots([42]);
    assert_eq!(usage.network.network_receive_bytes, 4_096);
    assert_eq!(usage.network.network_transmit_bytes, 2_048);
    assert_eq!(usage.network.network_receive_bytes_per_second, 2_048.0);
    assert_eq!(usage.network.network_transmit_bytes_per_second, 1_024.0);
    assert!(usage.measurement.network_bytes_available);
}

#[test]
fn includes_descendants_that_move_out_of_an_application_cgroup() {
    let socket_inode = 88;
    let process = |parent_pid, cpu_percent, sockets| ProcessUsage {
        parent_pid,
        cpu_percent,
        files: Arc::new(ProcessFiles {
            sockets,
            fd_available: true,
            ..ProcessFiles::default()
        }),
        ..ProcessUsage::default()
    };
    let path = "/user.slice/app-example.scope".to_owned();
    let snapshot = ResourceSnapshot {
        processes: HashMap::from([
            (10, process(1, 2.0, HashSet::new())),
            (11, process(10, 5.0, HashSet::from([socket_inode]))),
        ]),
        children: HashMap::from([(1, vec![10]), (10, vec![11])]),
        cgroup_members_by_root: HashMap::from([(10, HashSet::from([10]))]),
        cgroup_path_by_root: HashMap::from([(10, path.clone())]),
        cgroup_usage: HashMap::from([(
            path,
            super::CgroupUsage {
                cpu_percent: 2.0,
                ..super::CgroupUsage::default()
            },
        )]),
        network_deltas: HashMap::from([(
            socket_inode,
            NetworkCounters {
                received_bytes: 1_000,
                transmitted_bytes: 500,
            },
        )]),
        network_counters_available: true,
        logical_cpus: 1,
        interval_seconds: 2.0,
        ..ResourceSnapshot::default()
    };

    let usage = snapshot.usage_for_roots([10]);
    assert_eq!(usage.compute.process_count, 2);
    assert_eq!(usage.compute.cpu_percent, 7.0);
    assert_eq!(usage.network.network_receive_bytes_per_second, 500.0);
    assert_eq!(usage.network.network_transmit_bytes_per_second, 250.0);
    assert_eq!(usage.measurement.attribution_method, "mixed");
    assert!(usage.measurement.network_bytes_available);
}

#[test]
fn ignores_previous_counters_after_pid_reuse() {
    let sampler = super::ResourceSampler {
        previous_processes: HashMap::from([(
            42,
            super::PreviousProcess {
                total_ticks: 100,
                start_ticks: 7,
                major_faults: 10,
                io: Some(ProcessIo {
                    physical_read_bytes: 1_000,
                    physical_write_bytes: 2_000,
                    ..ProcessIo::default()
                }),
            },
        )]),
        ..Default::default()
    };
    let process = super::ProcessStat {
        parent_pid: 1,
        total_ticks: 500,
        start_ticks: 8,
        major_faults: 20,
        thread_count: 1,
    };
    let current = ProcessIo {
        physical_read_bytes: 4_000,
        physical_write_bytes: 8_000,
        ..ProcessIo::default()
    };
    assert_eq!(sampler.cpu_percent(42, &process, Some(100), 4), 0.0);
    assert_eq!(
        sampler.io_delta(42, &process, current).physical_read_bytes,
        0
    );
    assert_eq!(sampler.major_fault_delta(42, &process), 0);
}
