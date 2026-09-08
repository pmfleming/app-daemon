# Quality review and refactor

Measured with the local `../rust-quality-lens` checkout, using `rqlens.toml`.
Baseline artifacts are in `target/analysis-before/`; current artifacts are in
`target/analysis/` (both ignored build outputs).

## Initial refactor results (`e93acd4`)

The following measurements describe the initial refactor, before the subsequent
test-pruning pass documented below.

These are app-daemon measurements, not measurements of its dependency projects.
Complexity totals include test functions. Architecture averages are unweighted
module averages; adding a test module changes that population.

| Measurement | Before | After |
| --- | ---: | ---: |
| Maximum cognitive complexity | 19 | 11 |
| Total cognitive complexity | 373 | 340 |
| Maximum cyclomatic complexity | 14 | 10 |
| Total cyclomatic complexity | 926 | 901 |
| Worst function hotspot score (lower is better) | 115.92 | 73.81 |
| Mean module locality score (higher is better) | 99.67 | 99.84 |
| Mean module leverage score (higher is better) | 67.28 | 67.12 |
| Resource-module leverage score | 79.5 | 92.0 |
| Service-test locality / leverage | 94 / 47 | 100 / 62 |
| AST duplicate groups | 2 | 1 |
| Duplicated lines / duplication percentage | 415 / 5.92% | 411 / 5.86% |
| Reported escape hatches | 7 | 0 |
| Physical Rust lines under `src/` | 7,647 | 7,643 |
| Lines excluding dedicated test/provider files | 6,568 | 6,422 |
| Passing unit tests | 36 | 37 |

The production-file reduction is partly offset by regression coverage, explicit
imports, and formatting previously compressed tests. The non-test-file count
still includes inline test modules. Total nonblank lines remain 7,010.
Leverage did **not** improve uniformly: its project-wide mean is slightly lower.
Explicit imports also make dependency identity more visible to the analyzer.
This RQLens version does not expose Halstead effort; hotspot/complexity scores
are maintenance-pressure proxies, not measured engineering hours or runtime.

## Changes and rationale

- `src/resources.rs`: replace two TTL-cache implementations with one
  PID/start-time keyed `ProcessCache<T>`. Refresh missing identities immediately,
  evict inactive identities, and preserve separate memory/file deadlines.
- Share immutable socket sets with `Arc` instead of deep-copying the entire
  socket-ownership map on every sample. Retain startup, recovery, PID-reuse,
  descriptor-failure, and newly attributed process baselines.
- Reuse the already-built child-process map and sampled I/O map. Remove unused
  cgroup swap accumulation/readout, redundant process-parent storage, the
  deadline newtype, and the cloned attribution-root set. Process swap reporting
  remains intact.
- `src/resources/disk.rs`: separate bounded request scheduling from result
  publication; borrow target IDs until dispatch and preserve completed values
  on failed refreshes. Keep worker panic isolation and bounded channels.
- `src/resources/system.rs`: share regular-file discovery between fd/map-files
  scans, borrow procfs counter keys, avoid an intermediate CPU-counter vector,
  and distinguish missing roots from directory errors in a small helper.
- `src/service/query.rs`: normalize queries once per page, filter in place,
  reuse search fields for metadata/acronyms, and express direct-match precedence
  as ordered tiers. Colocate query tests in `src/service/query/tests.rs` rather
  than coupling operation-service tests to catalog/resource/settings fixtures.
- `src/history.rs`: extract expired buckets without cloning their IDs and use
  the same deque representation for persistence and live history. JSON arrays
  and file versions remain compatible.
- `src/settings.rs`, `src/history.rs`: reuse the existing framework atomic JSON
  writer instead of duplicate `.tmp` implementations. Settings serialization
  borrows the application map. **Intentional persistence hardening:** writes now
  use unique temporary files, fsync, private files (0600), and private parent
  directories (0700), including permission updates to existing store directories.
- Replace all seven test wildcard imports with explicit dependencies; no lint
  suppressions or unsafe code were introduced.

## Initial refactor verification and remaining work

Passed `cargo fmt --all -- --check`, strict Clippy (`--all-targets --locked --
-D warnings`), all 37 unit tests, doctests (none defined), and `git diff --check`.
RQLens `verify` also passed compilation, formatting, Clippy, tests, doctests,
and rustdoc, with zero error-level failures. Its five project-practice warnings
remain: missing declared MSRV, contributing guide, code of conduct, security
policy, and changelog. Fourteen optional/inapplicable checks were skipped;
coverage, mutation testing, auditing, and performance benchmarks were not run.

The resource sampler remains a long orchestration function (141 SLOC), and query
construction still couples catalog, settings, procfs identity, and presentation.
Those are follow-up design work, not solved by moving functions solely to change
scores. Remaining reliability findings are test panic-path advisories. No live
Hyprland/D-Bus or host resource-accounting integration run was performed.

## Subsequent test pruning

Reduced the suite from **37 to 25 tests (32.4%)**, removing **135 Rust lines**.
Application logic and current wire formats are unchanged; edits are confined to
tests, their provider fixture, and removal of an unused test-module declaration.

Selection was based on behavioral overlap, not simply combining unrelated tests:

| Overlapping group | Before → after | Retained behavior |
| --- | --- | --- |
| Cache internals and PID-reuse helpers | 3 → 1 | Sampler outputs for CPU, faults, I/O, memory, files, and sockets after PID reuse |
| GPU parsing | 2 → 1 | Standard counters and malformed input in one parser contract |
| GPU usage | 2 → 1 | Shared-engine aggregation, busy cap, and PID-reuse baseline with one fixture |
| Network accounting | 3 → 2 | Socket lifecycle plus recovery from missing counters/descriptors, through the sampler rather than private state |
| Blocked disk workers | 2 → 1 | Bounded outstanding scans and nonblocking sample publication in the same scenario |
| Directory walks | 2 → 1 | Hardlink/symlink rules and incomplete-walk rejection on one filesystem fixture |
| Resource/energy persistence | 2 → 1 | One round trip, final partial buckets, and actual two-day/eight-day retention checks |
| Legacy history deserialization | 1 → 0 | Removed the legacy-only compatibility constraint; current schema fixtures remain |
| Duplicate-root accounting | 2 → 1 | Process-tree and cgroup modes share one resource fixture |
| Revision precision and query results | 2 → 1 | JavaScript-safe revisions checked on the returned page instead of the hashing helper |
| Stale actions and operation lifecycle | 2 → 1 | Rejection emits no operation, followed by accepted/running/failed outcomes |

Removed cache-map/deadline assertions and memory/file read-count instrumentation
that unnecessarily pinned implementation choices. Kept the standalone current
API/resource fixtures, selector validation, correlation, cursor isolation,
catalog precedence, launch-only/UWSM identity, settings persistence, mixed
availability, scope-escape regression, and failed-disk-refresh checks.

Validation: all 25 tests pass, including **20 repeated library-suite runs**;
strict Clippy, formatting, doctests (none defined), and diff checks pass.

A `cargo llvm-cov --all-targets --locked --json` comparison against a separate
checkout of `e93acd4` also passed both suites. Coverage over the 22 non-test source
files rose from **2,737/4,613 lines (59.33%)** to **2,765/4,611 (59.97%)**.
`src/resources.rs` rose from **88.38% to 91.86%**, and `src/history.rs` from
**93.17% to 94.41%**. The file filter excludes dedicated test/provider files but
includes inline test code; the two-line denominator decrease is in GPU parser
test code. Line coverage is not branch/mutation coverage, and live daemon/launch
integration paths remain a separate coverage gap.

Coverage artifacts: `target/test-pruning-coverage-before.json` and
`target/test-pruning-coverage-after.json`. On this Nix toolchain, set `LLVM_COV`
and `LLVM_PROFDATA` to the installed LLVM 21 binaries when running cargo-llvm-cov.

Reproduce static evidence with:

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage module-cohesion; do
  "$RQL" measure "$tool" --config rqlens.toml
done
"$RQL" verify --config rqlens.toml
```
