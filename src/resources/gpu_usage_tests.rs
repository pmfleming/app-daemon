use super::super::{ProcessUsage, ResourceSampler, ResourceSnapshot};
use super::{GpuProcessStat, Usage, parse_gpu_fdinfo};
use std::collections::HashMap;

fn counter(value: u64) -> GpuProcessStat {
    let mut result = GpuProcessStat::default();
    for raw in [
        format!(
            "drm-pdev: 0000:03:00.0\ndrm-client-id: 1\ndrm-engine-gfx: {} ms\ndrm-memory-vram: 64 MiB\ndrm-resident-vram: 32 MiB\n",
            value * 3
        ),
        format!(
            "drm-pdev: 0000:03:00.0\ndrm-client-id: 2\ndrm-engine-gfx: {} ms\ndrm-engine-copy: {} ms\n",
            value * 4,
            value
        ),
    ] {
        let (id, client) = parse_gpu_fdinfo(&raw).unwrap();
        result.add_client(id, client);
    }
    result
}

fn sample(sampler: &mut ResourceSampler, start_ticks: u64, value: u64, seconds: f64) -> Usage {
    sampler
        .gpu
        .sample([((42, start_ticks), counter(value))], seconds)
        .remove(&42)
        .unwrap()
}

#[test]
fn drm_counters_aggregate_engines_and_rebaseline_on_identity_or_resume_changes() {
    assert!(parse_gpu_fdinfo("drm-engine-gfx: nope\n").is_none());
    assert!(parse_gpu_fdinfo("drm-client-id: 4\ndrm-engine-gfx: nope\n").is_none());
    let mut sampler = ResourceSampler::default();
    sample(&mut sampler, 1, 0, 0.0);
    let gpu = sample(&mut sampler, 1, 100, 1.0);
    assert!(gpu.available);
    assert_eq!(gpu.resident_bytes, 32 * 1024 * 1024);
    assert_eq!(gpu.allocated_bytes, 64 * 1024 * 1024);
    assert_eq!(gpu.engines["0000:03:00.0/gfx"], 70.0);
    assert_eq!(gpu.engines["0000:03:00.0/copy"], 10.0);
    let mut snapshot = ResourceSnapshot {
        processes: HashMap::from([
            (
                42,
                ProcessUsage {
                    gpu,
                    ..Default::default()
                },
            ),
            (
                43,
                ProcessUsage {
                    gpu: Usage {
                        engines: HashMap::from([
                            ("0000:03:00.0/gfx".into(), 20.0),
                            ("0000:04:00.0/gfx".into(), 60.0),
                        ]),
                        ..Default::default()
                    },
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
        .gpu
        .engines
        .insert("0000:03:00.0/gfx".into(), 40.0);
    let usage = snapshot.usage_for_roots([42, 43]);
    assert_eq!(usage.compute.gpu_busy_percent, 100.0);
    assert_eq!(usage.compute.gpu_percent, 180.0);
    let reused = sample(&mut sampler, 2, 200, 1.0);
    assert!(reused.engines.values().all(|v| *v == 0.0));
    sampler.reset_after_resume();
    let resumed = sample(&mut sampler, 2, 300, 1.0);
    assert!(resumed.engines.values().all(|v| *v == 0.0));
    // Missing observations (including exited processes) discard their baselines.
    assert!(sampler.gpu.sample([], 1.0).is_empty());
    let recovered = sample(&mut sampler, 2, 400, 1.0);
    assert!(recovered.engines.values().all(|v| *v == 0.0));
}
