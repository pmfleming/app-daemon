use super::*;

#[test]
fn sums_gpu_clients_by_engine_then_processes_by_application() {
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

    let snapshot = ResourceSnapshot {
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
}

#[test]
fn caps_gpu_busy_only_after_aggregating_and_ignores_reused_pids() {
    let snapshot = ResourceSnapshot {
        processes: HashMap::from([(
            42,
            ProcessUsage {
                gpu_engine_percent: HashMap::from([("gpu/gfx".into(), 120.0)]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let usage = snapshot.usage_for_roots([42]);
    assert_eq!(usage.compute.gpu_busy_percent, 100.0);
    assert_eq!(usage.compute.gpu_percent, 120.0);

    let mut sampler = ResourceSampler::default();
    sampler
        .previous_gpu_engines
        .insert((42, 1, "gpu/1/gfx".into()), 10);
    let gpu = GpuProcessStat {
        engine_nanoseconds: HashMap::from([("gpu/1/gfx".into(), 1_000_000_000)]),
        ..Default::default()
    };
    assert_eq!(
        sampler.gpu_percent(42, 2, Some(&gpu), 1.0, &mut HashMap::new())["gpu/gfx"],
        0.0
    );
}
