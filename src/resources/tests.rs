use super::{
    CgroupUsage, DiskFile, DiskFileId, MemoryUsage, ProcessFiles, ProcessIo, ProcessUsage,
    ResourceSnapshot, parse_process_stat,
};
use anyhow::Context;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[test]
fn descendant_traversal_prunes_rejected_branches_and_terminates_cycles() {
    let children = HashMap::from([(1, vec![2, 3]), (2, vec![4]), (3, vec![1])]);
    assert_eq!(
        super::system::descendants([0, 1], &children),
        HashSet::from([1, 2, 3, 4])
    );
    assert_eq!(
        super::system::descendants_where([1, 1], &children, |pid| pid != 2),
        HashSet::from([1, 3]),
    );
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
fn totals_resources_without_double_counting_roots_in_either_attribution_mode() {
    let file = |inode, bytes| {
        (
            DiskFileId { device: 1, inode },
            DiskFile {
                bytes,
                temporary: false,
            },
        )
    };
    let process = |cpu_percent,
                   memory_bytes,
                   disk_read_bytes,
                   disk_write_bytes,
                   open_files: HashMap<_, _>| ProcessUsage {
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
        }),
        storage_available: true,
        ..ProcessUsage::default()
    };
    let processes = HashMap::from([
        (
            10,
            process(
                2.0,
                100,
                10,
                20,
                HashMap::from([file(1, 1024), file(2, 2048)]),
            ),
        ),
        (
            11,
            process(3.5, 200, 30, 40, HashMap::from([file(1, 1024)])),
        ),
        (20, process(1.0, 50, 50, 60, HashMap::new())),
    ]);
    let children = HashMap::from([(1, vec![10, 20]), (10, vec![11])]);
    let mut snapshot = ResourceSnapshot {
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

    let path = "/user.slice/app-example.scope".to_owned();
    snapshot
        .cgroup_members_by_root
        .insert(10, HashSet::from([10, 11]));
    snapshot.cgroup_path_by_root.insert(10, path.clone());
    snapshot.cgroup_usage.insert(
        path,
        CgroupUsage {
            cpu_percent: Some(80.0),
            io: Some(super::CgroupIo {
                read_bytes: 4096,
                ..Default::default()
            }),
            memory_bytes: Some(8192),
        },
    );
    let usage = snapshot.usage_for_roots([10]);
    assert_eq!(usage.measurement.attribution_method, "cgroup");
    assert_eq!(usage.compute.cpu_percent, 80.0);
    assert_eq!(usage.storage.disk_read_bytes, 4096);
    assert_eq!(usage.compute.memory_cgroup_bytes, 8192);
    assert_eq!(snapshot.usage_for_roots([0, 10, 10]), usage);

    // A child leaving the scope is still part of the application, but partial
    // cgroup totals must no longer replace complete process-tree accounting.
    snapshot
        .cgroup_members_by_root
        .insert(10, HashSet::from([10]));
    let mixed = snapshot.usage_for_roots([10]);
    assert_eq!(mixed.compute.process_count, 2);
    assert_eq!(mixed.compute.cpu_percent, 5.5);
    assert_eq!(mixed.storage.disk_read_bytes, 40);
    assert_eq!(mixed.measurement.attribution_method, "mixed");
}
