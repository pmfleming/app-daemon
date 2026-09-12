use super::*;

#[test]
fn missing_cgroup_io_keeps_procfs_values_and_correct_availability() {
    let path = "/app-test.scope".to_owned();
    let mut snapshot = ResourceSnapshot {
        processes: HashMap::from([(
            42,
            ProcessUsage {
                cpu_percent: 3.0,
                io: ProcessIo {
                    physical_read_bytes: 123,
                    ..Default::default()
                },
                storage_available: true,
                ..Default::default()
            },
        )]),
        cgroup_members_by_root: HashMap::from([(42, HashSet::from([42]))]),
        cgroup_path_by_root: HashMap::from([(42, path.clone())]),
        cgroup_usage: HashMap::from([(
            path.clone(),
            CgroupUsage {
                cpu_percent: Some(25.0),
                memory_bytes: Some(512),
                io: None,
            },
        )]),
        ..Default::default()
    };
    let usage = snapshot.usage_for_roots([42]);
    assert_eq!(usage.compute.cpu_percent, 25.0);
    assert_eq!(usage.compute.memory_cgroup_bytes, 512);
    assert_eq!(usage.storage.disk_read_bytes, 123);
    assert!(usage.measurement.storage_available);
    snapshot.processes.get_mut(&42).unwrap().storage_available = false;
    assert!(!snapshot.usage_for_roots([42]).measurement.storage_available);
    snapshot.cgroup_usage.get_mut(&path).unwrap().io = Some(CgroupIo::default());
    let idle = snapshot.usage_for_roots([42]);
    assert!(idle.measurement.storage_available);
    assert_eq!(idle.storage.disk_read_bytes, 0);
    snapshot.cgroup_usage.get_mut(&path).unwrap().cpu_percent = None;
    assert_eq!(snapshot.usage_for_roots([42]).compute.cpu_percent, 3.0);
}

#[test]
fn recovered_cgroup_controllers_baseline_independently() {
    let path = "app.scope".to_owned();
    let mut sampler = ResourceSampler::default();
    sampler.previous_cgroups.insert(
        path.clone(),
        CgroupCounters {
            cpu_usage_usec: Some(1_000_000),
            ..Default::default()
        },
    );
    let current = HashMap::from([(
        path.clone(),
        CgroupCounters {
            cpu_usage_usec: Some(2_000_000),
            memory_bytes: Some(512),
            io: Some(CgroupIo {
                read_bytes: 10_000,
                ..Default::default()
            }),
        },
    )]);
    let usage = sampler.cgroup_usage(&current, 1.0);
    assert_eq!(usage[&path].cpu_percent, Some(100.0));
    assert_eq!(usage[&path].memory_bytes, Some(512));
    assert_eq!(usage[&path].io.unwrap().read_bytes, 0);
    sampler.previous_cgroups = current.clone();
    assert_eq!(
        sampler.cgroup_usage(&current, 1.0)[&path]
            .io
            .unwrap()
            .read_bytes,
        0
    );
}
