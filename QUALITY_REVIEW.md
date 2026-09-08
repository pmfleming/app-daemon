# Quality review and refactor

Measured with the local `../rust-quality-lens` checkout, using `rqlens.toml`.
Baseline artifacts are in `target/analysis-before/`; current artifacts are in
`target/analysis/` (both ignored build outputs).

## Results

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

## Verification and remaining work

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

Reproduce static evidence with:

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage module-cohesion; do
  "$RQL" measure "$tool" --config rqlens.toml
done
"$RQL" verify --config rqlens.toml
```
