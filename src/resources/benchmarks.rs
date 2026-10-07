use super::{
    ResourceSampler,
    disk::WORKERS,
    provider::{DiskBreakdown, MemoryUsage, ProcessIo, ProcessStat},
    test_provider::TestProvider,
};
use crate::benchmarks::{Measurement, measure};
use std::{
    collections::HashMap,
    hint::black_box,
    sync::{Arc, Barrier, mpsc},
    time::Duration,
};

pub(crate) fn run(iterations: usize) -> anyhow::Result<Vec<Measurement>> {
    let provider = Arc::new(TestProvider::default());
    {
        let mut state = provider.state.lock().unwrap();
        for pid in 1..=1000 {
            state.processes.insert(
                pid,
                ProcessStat {
                    parent_pid: 0,
                    total_ticks: 100,
                    start_ticks: 1,
                    major_faults: 0,
                    thread_count: 1,
                },
            );
            state.memory.insert(
                pid,
                MemoryUsage {
                    rss_bytes: 4096,
                    rss_available: true,
                    ..Default::default()
                },
            );
            state.io.insert(pid, ProcessIo::default());
        }
        state.disk_usage = Some(DiskBreakdown {
            total_bytes: 4096,
            ..Default::default()
        });
    }
    let targets: HashMap<_, _> = (0..64)
        .map(|index| {
            (
                format!("app-{index}.desktop"),
                (index * 4 + 1..=index * 4 + 4).collect::<Vec<_>>(),
            )
        })
        .collect();
    let mut sampler = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = sampler.sample_for_targets(&targets);
        if targets.iter().all(|(target, pids)| {
            snapshot
                .usage_for_target(target, pids.iter().copied())
                .measurement
                .disk_space_scope
                == "identified-app-directories"
        }) {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "disk cache warmup timed out"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let warm = measure("sampling/warm", iterations, || {
        sample(&mut sampler, &provider, &targets)
    });

    drop(sampler);
    // Separate provider state prevents any finishing warm-cache worker from
    // entering the blocked scenario's barrier.
    let provider = Arc::new(TestProvider {
        state: std::sync::Mutex::new(provider.state.lock().unwrap().clone()),
        ..Default::default()
    });
    let gate = Arc::new(Barrier::new(WORKERS + 1));
    *provider.disk_gate.lock().unwrap() = Some(gate.clone());
    let (started, receiver) = mpsc::channel();
    *provider.disk_started.lock().unwrap() = Some(started);
    // A fresh cache requests disk scans; wait for both workers before timing.
    let mut blocked = ResourceSampler {
        provider: provider.clone(),
        ..Default::default()
    };
    blocked.sample_for_targets(&targets);
    for _ in 0..WORKERS {
        receiver.recv_timeout(Duration::from_secs(5))?;
    }
    let busy = measure("sampling/blocked-disk-workers", iterations, || {
        sample(&mut blocked, &provider, &targets)
    });
    *provider.disk_gate.lock().unwrap() = None;
    gate.wait();
    Ok(vec![warm, busy])
}

fn sample(
    sampler: &mut ResourceSampler,
    provider: &TestProvider,
    targets: &HashMap<String, Vec<u32>>,
) {
    {
        let mut state = provider.state.lock().unwrap();
        state.system_ticks += 1000;
        for process in state.processes.values_mut() {
            process.total_ticks += 1;
        }
    }
    let snapshot = sampler.sample_for_targets(targets);
    for (target, pids) in targets {
        black_box(snapshot.usage_for_target(target, pids.iter().copied()));
    }
}
