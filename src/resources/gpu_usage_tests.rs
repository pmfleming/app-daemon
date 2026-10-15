use super::super::{ProcessUsage, ResourceSampler, ResourceSnapshot};
use super::{aggregate_gpu_clients, parse_gpu_fdinfo};
use std::collections::HashMap;

#[test]
fn drm_counters_aggregate_engines_and_rebaseline_on_identity_or_resume_changes() {
    assert!(parse_gpu_fdinfo("drm-engine-gfx: nope\n").is_none());
    assert!(parse_gpu_fdinfo("drm-client-id: 4\ndrm-engine-gfx: nope\n").is_none());
    let counter = |value: u64| {
        aggregate_gpu_clients([
            format!("drm-pdev: 0000:03:00.0\ndrm-client-id: 1\ndrm-engine-gfx: {} ms\ndrm-memory-vram: 64 MiB\ndrm-resident-vram: 32 MiB\n", value * 3),
            format!("drm-pdev: 0000:03:00.0\ndrm-client-id: 2\ndrm-engine-gfx: {} ms\ndrm-engine-copy: {} ms\n", value * 4, value),
        ].iter().map(|raw| parse_gpu_fdinfo(raw).unwrap()).collect())
    };
    let mut sampler = ResourceSampler::default();
    let mut next = HashMap::new();
    sampler.gpu_percent(42, 1, Some(&counter(0)), 0.0, &mut next);
    sampler.previous_gpu_engines = next;
    let current = counter(100);
    assert_eq!(current.resident_memory_bytes, 32 * 1024 * 1024);
    assert_eq!(current.allocated_memory_bytes, 64 * 1024 * 1024);
    let engines = sampler.gpu_percent(42, 1, Some(&current), 1.0, &mut HashMap::new());
    assert_eq!(engines["0000:03:00.0/gfx"], 70.0);
    assert_eq!(engines["0000:03:00.0/copy"], 10.0);
    let mut snapshot = ResourceSnapshot {
        processes: HashMap::from([
            (
                42,
                ProcessUsage {
                    gpu_engine_percent: engines,
                    ..Default::default()
                },
            ),
            (
                43,
                ProcessUsage {
                    gpu_engine_percent: HashMap::from([
                        ("0000:03:00.0/gfx".into(), 20.0),
                        ("0000:04:00.0/gfx".into(), 60.0),
                    ]),
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    };
    let usage = snapshot.usage_for_roots([42, 43]);
    assert_eq!(usage.compute.gpu_busy_percent, 90.0);
    assert_eq!(usage.compute.gpu_percent, 160.0);
    snapshot
        .processes
        .get_mut(&43)
        .unwrap()
        .gpu_engine_percent
        .insert("0000:03:00.0/gfx".into(), 40.0);
    let usage = snapshot.usage_for_roots([42, 43]);
    assert_eq!(usage.compute.gpu_busy_percent, 100.0);
    assert_eq!(usage.compute.gpu_percent, 180.0);
    let reused = sampler.gpu_percent(42, 2, Some(&current), 1.0, &mut HashMap::new());
    assert!(reused.values().all(|percent| *percent == 0.0));
    sampler.reset_after_resume();
    let resumed = sampler.gpu_percent(42, 1, Some(&current), 1.0, &mut HashMap::new());
    assert!(resumed.values().all(|percent| *percent == 0.0));
}
