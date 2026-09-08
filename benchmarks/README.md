# Synthetic performance baseline

```sh
cargo bench --features benchmarks --bench workloads --locked > target/workloads.json
# Optional: APP_DAEMON_BENCH_ITERS=1000 (default 500; accepted range 1..10000)
```

The feature is opt-in: normal builds do not include benchmark fixtures or replace
the global allocator. It enables `allocation-counter`, which measures allocations
on the calling thread. Timing and allocation phases are separate, but both use
that allocator; compare runs with the same compiler, feature, profile, and host.
Setup and warmup are outside measurements. The warm disk cache is fully populated
before timing. No runtime performance thresholds are enforced on noisy CI hosts.

`baseline.json` records three release runs. Each latency phase has 500 iterations,
followed by 500 allocation-count iterations; allocation counts were identical in
all three runs. Approximate median-of-medians:

| Workload | Median | Allocations/call | Allocated bytes/call |
| --- | ---: | ---: | ---: |
| Empty query | 767 µs | 22,801 | 2,312,402 |
| Search query | 1,098 µs | 28,214 | 1,811,876 |
| Identity, no cgroup | 504 µs | 200 | 3,200 |
| Warm sampler + target summaries | 369 µs | 2,613 | 1,453,865 |
| Same sampler with disk workers blocked | 361 µs | 2,548 | 1,440,567 |

Queries use 1,000 entries and 200 windows with unavailable resource samples.
They include cloning pre-resolved grouping but exclude procfs, D-Bus, compositor
and service-lock latency. Identity matching is measured separately without
cgroups. Sampling uses 1,000 processes, 256 active processes, and 64 targets.
Counters advance on every iteration; their fixture updates and provider map
cloning are included. GPU/socket-heavy workloads and actual procfs traversal are
not represented by this fixture.

The blocked workload holds every disk worker at a barrier throughout measurement;
publication still completes. Its disk-space results are unavailable, so it does
less result-map copying than the warm workload. The small timing difference is
**not** evidence that blocking workers speeds up sampling. It measures isolation,
not real filesystem throughput or worst-case kernel syscall latency.

No optimization was applied to make these numbers look better. Candidates for a
subsequent measured pass are repeated query sort-key allocation, linear catalog
identity scans, and allocation of per-process summaries for inactive processes.
Keep the fixtures fixed when comparing such changes. This is a baseline, not a
before/after speedup claim or an end-to-end desktop benchmark.
