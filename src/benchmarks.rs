//! Opt-in synthetic workload measurements; not part of the daemon's normal build.
use serde::Serialize;
use std::{hint::black_box, time::Instant};

#[derive(Serialize)]
pub(crate) struct Measurement {
    name: &'static str,
    iterations: usize,
    median_us: f64,
    p95_us: f64,
    max_us: f64,
    allocations_per_iteration: f64,
    allocated_bytes_per_iteration: f64,
}

pub(crate) fn measure(name: &'static str, iterations: usize, mut run: impl FnMut()) -> Measurement {
    for _ in 0..20 {
        run();
    }
    let mut elapsed = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        run();
        elapsed.push(start.elapsed().as_nanos() as u64);
    }
    let allocations = allocation_counter::measure(|| {
        for _ in 0..iterations {
            run();
        }
    });
    elapsed.sort_unstable();
    Measurement {
        name,
        iterations,
        median_us: elapsed[iterations / 2] as f64 / 1000.0,
        p95_us: elapsed[(iterations * 95 / 100).min(iterations - 1)] as f64 / 1000.0,
        max_us: elapsed[iterations - 1] as f64 / 1000.0,
        allocations_per_iteration: allocations.count_total as f64 / iterations as f64,
        allocated_bytes_per_iteration: allocations.bytes_total as f64 / iterations as f64,
    }
}

/// Run reproducible query, identity, and sampling workloads and return JSON evidence.
/// Latencies use the allocation-counter allocator; allocation counts cover only
/// the calling thread, not asynchronous disk workers. Fixture setup is excluded.
pub fn run(iterations: usize) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        (1..=10_000).contains(&iterations),
        "iterations must be in 1..=10000"
    );
    let mut measurements = crate::service::benchmarks::run(iterations)?;
    measurements.extend(crate::resources::benchmarks::run(iterations)?);
    Ok(serde_json::json!({
        "schema_version": 1,
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "fixture": { "catalog_entries": 1000, "windows": 200, "processes": 1000, "active_processes": 256, "targets": 64 },
        "allocation_scope": "calling thread; includes synthetic provider and counter updates",
        "measurements": black_box(measurements),
    }))
}
