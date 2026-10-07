use super::{
    ResourceSampler,
    provider::{CgroupCounters, NetworkCounters},
    test_provider::TestProvider,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

#[test]
fn counter_reset_on_resume_is_a_baseline_not_a_wrap_or_history_interval() {
    let provider = Arc::new(TestProvider::default());
    let path = PathBuf::from("package-0");
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(path.clone(), (900_000_000, 1_000_000_000));
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let roots = HashMap::new();
    sampler.sample_for_targets(&roots);
    // All rate families share the same discontinuity boundary.
    sampler
        .previous_gpu_engines
        .insert((1, 1, "gfx".into()), 900_000_000);
    sampler
        .previous_cgroups
        .insert("app.scope".into(), CgroupCounters::default());
    sampler.previous_network_counters.insert(
        1,
        NetworkCounters {
            received_bytes: 10_000,
            transmitted_bytes: 500,
        },
    );
    sampler.reset_after_resume();
    assert!(sampler.previous_gpu_engines.is_empty());
    assert!(sampler.previous_cgroups.is_empty());
    assert!(sampler.previous_network_counters.is_empty());
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(path.clone(), (100, 1_000_000_000));
    let baseline = sampler.sample_for_targets(&roots);
    assert_eq!(
        baseline.interval_seconds(),
        0.0,
        "history ignores this unobserved interval"
    );
    assert_eq!(baseline.system_energy_mwh, 0.0);
    assert_eq!(baseline.energy_source, "unavailable");
    provider
        .state
        .lock()
        .unwrap()
        .rapl
        .insert(path, (7_200_100, 1_000_000_000));
    let next = sampler.sample_for_targets(&roots);
    assert!(next.interval_seconds() > 0.0);
    assert_eq!(
        next.system_energy_mwh, 2.0,
        "only post-resume work is counted"
    );
    assert_eq!(next.energy_source, "rapl");
}
