use super::{ProcessUsage, ResourceSampler, ResourceSnapshot, provider::GpuProcessStat};
use std::collections::HashMap;

#[test]
fn aggregates_gpu_engines_caps_busy_usage_and_rebaselines_reused_pids() {
    let mut sampler = ResourceSampler::default();
    let counter = |value| GpuProcessStat {
        engine_nanoseconds: HashMap::from([
            ("0000:03:00.0/1/gfx".into(), value * 3),
            ("0000:03:00.0/2/gfx".into(), value * 4),
            ("0000:03:00.0/2/copy".into(), value),
        ]),
        ..Default::default()
    };
    let mut next = HashMap::new();
    sampler.gpu_percent(42, 1, Some(&counter(0)), 0.0, &mut next);
    sampler.previous_gpu_engines = next;
    let engines = sampler.gpu_percent(42, 1, Some(&counter(100_000_000)), 1.0, &mut HashMap::new());
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
    let reused = sampler.gpu_percent(42, 2, Some(&counter(100_000_000)), 1.0, &mut HashMap::new());
    assert!(reused.values().all(|percent| *percent == 0.0));
}
