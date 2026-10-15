use super::{
    CgroupUsage, ProcessUsage, ResourceSnapshot,
    provider::{
        DiskBreakdown, DiskFile, DiskFileId, IoCounters, MemoryUsage, ProcessFiles, ProcessIo,
    },
    system::parse_process_stat,
};
use anyhow::Context;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[test]
fn parses_proc_counters_and_live_identity_without_trusting_command_names() -> anyhow::Result<()> {
    let stat = "42 (name with ) spaces) S 7 0 0 0 0 0 0 0 17 0 120 30 0 0 0 0 5 0 99 0 0";
    #[cfg(feature = "benchmarks")]
    {
        let allocations = allocation_counter::measure(|| {
            std::hint::black_box(parse_process_stat(stat));
            std::hint::black_box(crate::process::parse_process(stat));
        });
        assert_eq!(allocations.count_total, 0);
    }
    let identity = crate::process::parse_process(stat).context("live identity")?;
    assert_eq!((identity.parent, identity.start), (7, 99));
    for invalid in [stat.replace(") S", ") Z"), "malformed".into()] {
        assert!(crate::process::parse_process(&invalid).is_none());
    }
    let process = parse_process_stat(stat).context("valid stat")?;
    assert_eq!(process.parent_pid, 7);
    assert_eq!(process.total_ticks, 150);
    assert_eq!(process.start_ticks, 99);
    assert_eq!(process.major_faults, 17);
    assert_eq!(process.thread_count, 5);
    let fields = crate::process::process_stat_fields(stat)
        .context("stat fields")?
        .collect::<Vec<_>>();
    for end in 0..20 {
        let truncated = format!("42 (command) {}", fields[..end].join(" "));
        assert!(parse_process_stat(&truncated).is_none());
        assert!(crate::process::parse_process(&truncated).is_none());
    }
    let saturated = stat.replace("120 30", &format!("{} 30", u64::MAX));
    assert_eq!(
        parse_process_stat(&saturated)
            .context("large counters")?
            .total_ticks,
        u64::MAX
    );
    assert!(parse_process_stat(&stat.replace("120 30", "invalid 30")).is_none());
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
            storage: IoCounters {
                read_bytes: disk_read_bytes,
                write_bytes: disk_write_bytes,
                ..Default::default()
            },
            ..ProcessIo::default()
        },
        files: Some(Arc::new(ProcessFiles {
            referenced: open_files.clone(),
            open: open_files,
            fd_available: true,
        })),
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
    assert!(usage.metric_availability.0["cpu_percent"]);
    assert!(usage.metric_availability.0["energy_mwh"]);
    assert!(!usage.metric_availability.0["disk_space_total_bytes"]);
    snapshot.app_disk_by_target.insert(
        "cached".into(),
        DiskBreakdown {
            total_bytes: 7,
            temporary_bytes: 2,
            permanent_bytes: 5,
        },
    );
    for (target, available, bytes) in [("cached", true, 7), ("missing", false, 0)] {
        let usage = snapshot.usage_for_target(target, [10]);
        assert_eq!(
            usage.metric_availability.0["disk_space_total_bytes"],
            available
        );
        assert_eq!(usage.storage.disk_space_total_bytes, bytes);
        assert!(usage.metric_availability.0["cpu_percent"]);
        assert!(usage.metric_availability.0["energy_mwh"]);
    }

    let path = "/user.slice/app-example.scope".to_owned();
    snapshot
        .cgroup_members_by_path
        .insert(path.clone(), HashSet::from([10, 11]));
    snapshot.cgroup_path_by_root.insert(10, path.clone());
    snapshot.cgroup_usage.insert(
        path.clone(),
        CgroupUsage {
            cpu_percent: Some(80.0),
            io: Some(IoCounters {
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
    snapshot.cgroup_path_by_root.insert(11, path.clone());
    assert_eq!(snapshot.usage_for_roots([10, 11]), usage);

    // A child leaving the scope is still part of the application, but partial
    // cgroup totals must no longer replace complete process-tree accounting.
    snapshot
        .cgroup_members_by_path
        .insert(path, HashSet::from([10]));
    let mixed = snapshot.usage_for_roots([10]);
    assert_eq!(mixed.compute.process_count, 2);
    assert_eq!(mixed.compute.cpu_percent, 5.5);
    assert_eq!(mixed.storage.disk_read_bytes, 40);
    assert_eq!(mixed.measurement.attribution_method, "mixed");
}
