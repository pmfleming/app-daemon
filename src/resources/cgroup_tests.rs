use super::{CgroupCounters, CgroupIo, ProcessIo, ProcessUsage, ResourceSampler, ResourceSnapshot};
use std::collections::{HashMap, HashSet};

#[test]
fn unavailable_controllers_fall_back_then_recover_without_lifetime_spikes() {
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
        ..Default::default()
    };
    let mut sampler = ResourceSampler::default();
    let mut counters = HashMap::from([(
        path.clone(),
        CgroupCounters {
            cpu_usage_usec: Some(1_000_000),
            memory_bytes: Some(512),
            io: None,
        },
    )]);
    sampler.previous_cgroups = counters.clone();
    counters.get_mut(&path).unwrap().cpu_usage_usec = Some(1_250_000);
    snapshot.cgroup_usage = sampler.cgroup_usage(&counters, 1.0);
    let fallback = snapshot.usage_for_roots([42]);
    assert_eq!(fallback.compute.cpu_percent, 25.0);
    assert_eq!(fallback.compute.memory_cgroup_bytes, 512);
    assert_eq!(fallback.storage.disk_read_bytes, 123);
    assert!(fallback.measurement.storage_available);
    snapshot.processes.get_mut(&42).unwrap().storage_available = false;
    assert!(!snapshot.usage_for_roots([42]).measurement.storage_available);

    sampler.previous_cgroups = counters.clone();
    counters.get_mut(&path).unwrap().io = Some(CgroupIo {
        read_bytes: 10_000,
        ..Default::default()
    });
    snapshot.cgroup_usage = sampler.cgroup_usage(&counters, 1.0);
    let recovered = snapshot.usage_for_roots([42]);
    assert!(recovered.measurement.storage_available);
    assert_eq!(
        recovered.storage.disk_read_bytes, 0,
        "recovery must establish an I/O baseline"
    );
    sampler.previous_cgroups = counters.clone();
    snapshot.cgroup_usage = sampler.cgroup_usage(&counters, 1.0);
    assert_eq!(snapshot.usage_for_roots([42]).storage.disk_read_bytes, 0);
    counters
        .get_mut(&path)
        .unwrap()
        .io
        .as_mut()
        .unwrap()
        .read_bytes += 50;
    counters.get_mut(&path).unwrap().cpu_usage_usec = None;
    snapshot.cgroup_usage = sampler.cgroup_usage(&counters, 1.0);
    let next = snapshot.usage_for_roots([42]);
    assert_eq!(next.storage.disk_read_bytes, 50);
    assert_eq!(
        next.compute.cpu_percent, 3.0,
        "missing CPU controller uses procfs"
    );
}
