use super::*;

#[test]
fn duplicate_window_pids_preserve_cgroup_totals() {
    let path = "/user.slice/app-example.scope".to_owned();
    let snapshot = ResourceSnapshot {
        processes: HashMap::from([(42, ProcessUsage {
            cpu_percent: 2.0,
            ..Default::default()
        })]),
        cgroup_members_by_root: HashMap::from([(42, HashSet::from([42]))]),
        cgroup_path_by_root: HashMap::from([(42, path.clone())]),
        cgroup_usage: HashMap::from([(path, CgroupUsage {
            cpu_percent: 80.0,
            read_bytes: 4096,
            memory_bytes: 8192,
            ..Default::default()
        })]),
        interval_seconds: 2.0,
        ..Default::default()
    };
    let single = snapshot.usage_for_roots([42]);
    assert_eq!(single.measurement.attribution_method, "cgroup");
    assert_eq!(single.compute.cpu_percent, 80.0);
    assert_eq!(single.storage.disk_read_bytes, 4096);
    assert_eq!(single.compute.memory_cgroup_bytes, 8192);
    assert_eq!(snapshot.usage_for_roots([0, 42, 42]), single);
}
