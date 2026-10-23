# Five priority improvements

Starting point: `f249edc`. Each step is implemented, validated and committed
separately. Local RQLens 0.1.0, complexity v2 / Halstead v1 / architecture v4,
with unchanged `quality/regression.toml` and policy. Compact paired snapshots
and benchmark/test logs live under `target/quality-five/` (ignored).
`quality/regression-baseline.json` is not replaced.

Measurements are authored-source heuristics, not correctness proofs. Inventory
remains partial for `declare_methods!`; semantic dependency resolution is complete
for collected references. Added helpers contribute to complexity sums, including
the base cyclomatic count of one even for a branchless constructor.

## 1. Defer availability projection during acquisition

`ResourceUsage::unprojected` initializes acquisition without a 48-key map;
`ProcessAggregation` uses it instead of the public default. Projection still occurs
only after target-specific disk metadata is finalized. Public `Default`, missing-field
Serde defaults and JSON shape are unchanged. A new regression checks those contracts
and zero allocations for aggregate construction with the benchmarks feature.

Synthetic release workloads, 100 iterations; counts are per iteration on the
calling thread, not worker allocations or a latency guarantee:

| Workload | Allocations before → after | Bytes before → after |
| --- | ---: | ---: |
| Empty query | 102,004 → 60,704 | 12,932,634 → 7,149,934 |
| Search query | 19,498 → 18,200 | 1,195,558 → 1,013,816 |
| Warm sampling | 9,093 → 5,317 | 2,181,459 → 1,652,755 |
| Blocked-worker sampling | 9,028 → 5,252 | 2,168,161 → 1,639,457 |

Tradeoffs: production cognitive sum stays 549; cyclomatic sum 1,242 → 1,244,
effort 4,409,904 → 4,412,810. Production-file lines grow 30, all Rust lines 60
(including the regression). Locality mean 98.4669 → 98.4559 because the test exposes
another model consumer (model locality 92.5 → 91.75). Leverage, duplicate lines,
escape hatches and clone counts are unchanged. This is an allocation improvement,
not a claim that every static gate improves.

Validation: 33 library and 15 session tests pass, five existing ignored registrations
unchanged; all-target/all-feature Clippy with `-D warnings`, formatting and diff
checks pass. RQLens paired snapshots: `before.json` → `step1.json`.
