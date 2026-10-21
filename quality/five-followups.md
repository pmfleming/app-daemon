# Five follow-up improvements

Starting point: `5bc7584` (separate checkpoint of the preceding refactor).
Measurements use the unchanged `quality/regression.toml`, local RQLens binary,
and `target/next-five-{0..5}.json` snapshots. The historical policy baseline is
not replaced. Macro-expansion limitations from the preceding review still apply.

## 1. Defer query hydration

`service/query.rs` now ranks lightweight candidates, then aggregates application
and window resources only for selected rows. Metadata ranking still sees the
original client titles/classes; running state still includes background processes.
The candidate does not allocate a placeholder resource-availability map. Identity
metadata remains eager; this does not claim zero allocation for discarded rows.

A regression test covers window-title search, catalog/unmatched rows, category
filtering, pagination, focus and complete selected-row payloads. All-feature tests
and strict all-target/all-feature Clippy pass.

Release synthetic benchmark, 30 iterations, same fixture and calling-thread
allocator instrumentation (timings are advisory):

| Query | Allocations before → after | Bytes before → after | Median µs before → after |
| --- | ---: | ---: | ---: |
| Empty (500 of 1,000 rows) | 165,003 → 102,004 | 21,335,634 → 12,940,634 | 10,255 → 5,297 |
| Search | 169,037 → 20,498 | 21,613,732 → 1,335,958 | 9,816 → 1,278 |

RQLens tradeoffs: production cyclomatic sum 1,199 → 1,200 and effort
4,365,912 → 4,375,248; cognitive sum stays 540. Production-file lines +23;
regression tests +62. Mean leverage improves 24.7761 → 25.3731, locality declines
98.5560 → 98.4552 (additional test dependencies and catalog fan-in). These static
costs buy measured allocation reductions, not a universal metric improvement.
