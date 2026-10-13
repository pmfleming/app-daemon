use super::read_cgroup_counters_at;
use crate::resources::{ProcessUsage, ResourceSampler, ResourceSnapshot, provider::ProcessIo};
use std::{
    collections::{HashMap, HashSet},
    fs,
};

#[test]
fn missing_or_invalid_controllers_fall_back_and_recovery_rebaselines_counters() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    assert!(read_cgroup_counters_at(root).is_none());
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
    let read = || HashMap::from([(path.clone(), read_cgroup_counters_at(root).unwrap())]);
    fs::write(root.join("cpu.stat"), "usage_usec 1000000\n")?;
    fs::write(root.join("memory.current"), "512\n")?;
    sampler.previous_cgroups = read();
    fs::write(root.join("cpu.stat"), "usage_usec 1250000\n")?;
    snapshot.cgroup_usage = sampler.cgroup_usage(&read(), 1.0);
    let fallback = snapshot.usage_for_roots([42]);
    assert_eq!(fallback.compute.cpu_percent, 25.0);
    assert_eq!(fallback.compute.memory_cgroup_bytes, 512);
    assert_eq!(fallback.storage.disk_read_bytes, 123);
    assert!(fallback.measurement.storage_available);

    snapshot.processes.get_mut(&42).unwrap().storage_available = false;
    assert!(!snapshot.usage_for_roots([42]).measurement.storage_available);
    sampler.previous_cgroups = read();
    fs::write(
        root.join("io.stat"),
        "8:0 rbytes=10000 wbytes=20 rios=1 wios=2\n",
    )?;
    snapshot.cgroup_usage = sampler.cgroup_usage(&read(), 1.0);
    let recovered = snapshot.usage_for_roots([42]);
    assert!(recovered.measurement.storage_available);
    assert_eq!(
        recovered.storage.disk_read_bytes, 0,
        "recovery must establish a baseline"
    );
    sampler.previous_cgroups = read();
    fs::write(
        root.join("io.stat"),
        "8:0 rbytes=10050 wbytes=30 rios=2 wios=3\n",
    )?;
    fs::remove_file(root.join("cpu.stat"))?;
    snapshot.cgroup_usage = sampler.cgroup_usage(&read(), 1.0);
    let next = snapshot.usage_for_roots([42]);
    assert_eq!(next.storage.disk_read_bytes, 50);
    assert_eq!(next.storage.disk_write_bytes, 10);
    assert_eq!(
        next.compute.cpu_percent, 3.0,
        "missing CPU controller uses procfs"
    );

    fs::write(root.join("io.stat"), "8:0 rbytes=invalid\n")?;
    assert!(read_cgroup_counters_at(root).unwrap().io.is_none());
    snapshot.cgroup_usage = sampler.cgroup_usage(&read(), 1.0);
    assert!(!snapshot.usage_for_roots([42]).measurement.storage_available);
    fs::write(root.join("io.stat"), "")?;
    let idle = read_cgroup_counters_at(root).unwrap();
    assert!(idle.cpu_usage_usec.is_none());
    assert_eq!(
        idle.io.unwrap().read_bytes,
        0,
        "idle is measured zero, not unavailable"
    );
    Ok(())
}
