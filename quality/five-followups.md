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

## 2. Stream history selection and summaries

`history::window_points` centralizes endpoint/overlap selection and returns a
borrowed double-ended iterator. `summary::summarize` consumes that iterator once,
tracking the same count/last endpoint for its revision. Queries no longer build a
`Vec<&ResourceHistoryPoint>` proportional to the retained window; only returned
page payloads are cloned. This is a source-level allocation guarantee, not a
measured latency claim.

Extended the cursor/restart regression with a summary-only right-boundary overlap:
it contributes observed duration but neither advances pagination nor changes the
summary on an exhausted page. Full all-feature tests passed before extracting the
selection helper; history tests and strict Clippy passed again afterward.

Step-local production effort falls 4,375,248 → 4,368,664; cognitive stays 540,
cyclomatic adds one helper (1,200 → 1,201). Production-file lines +4 and test lines
+18. All-code effort rises due to added assertions in the existing long cursor
scenario. Locality, leverage, clone-site counts and escape hatches are unchanged.
