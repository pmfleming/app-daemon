# Discovery, query and retention refactor

Baseline: `2fdf74c`. Measured with the local `../rust-quality-lens` checkout
(built with `cargo build --locked --bin rqlens`), not historical report values.

## Changes

- **Discovery:** colocate cgroup selection and member discovery in
  `src/resources/discovery.rs`. Both catalog-backed discovery and synthetic
  sampling now share the root-reading pipeline. Catalog identity still gates
  charging a whole cgroup. Remove discovery's dependency on the Linux provider
  implementation and reuse the canonical process-identity alias.
- **Queries:** borrow desktop actions until a candidate survives filtering and
  pagination. Build metadata/acronym search text without collecting a vector of
  field references; preallocate the corpus using a second borrowed traversal.
  Preserve field order, substring offsets, Unicode lowercasing and ranking tiers.
  Extend the existing query regression with action payloads, trimmed metadata,
  acronym and noncontiguous-term matches.
- **History:** share retention pruning, including empty-key removal and dirty
  generations, between resource and energy histories. Final saves expire all
  pending buckets through the existing insertion/repair path. Remove the separate
  final-flush implementation. This trades the old whole-map moves at shutdown for
  temporary extraction vectors; it is not a persistence-allocation improvement.
- **Watchers:** remove the redundant `failed` forwarding method and merge identical
  rebuild arms. Handle stream closure with other connection-state transitions.
  Debouncing remains inside the selected branch, preserving cancellation behavior.
- **Sampling/launch:** reduce RAPL deltas directly into an optional total (no
  temporary vector), parse/compare battery file text without owned trimmed copies,
  and build launch roots directly as a set instead of vector-then-set.
- **Settings:** reuse `metrics::revision` and derived map/settings hashing instead
  of another manual hasher. Opaque settings/page revision values change; persistence
  and API schemas do not. Tokens are not a persisted hash-format contract.
- Replace the persistence test's wildcard import with explicit dependencies.
  No tests, public APIs, lint checks or safety checks were removed. Removed helpers
  were redundant, not evidence that all potentially unused public code is gone.

## Paired RustQualityLens results

Scope follows `scripts/quality_regression.py`: authored app-daemon Rust, with
production functions separated by its existing path/name convention. Architecture
means cover the same 68-module inventory. Higher locality/leverage is better;
Halstead effort is a syntax metric, not developer hours or runtime cost.

| Measurement | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 555 / 8 | 549 / 8 |
| Production cyclomatic sum / max | 1,249 / 15 | 1,242 / 15 |
| Production function Halstead effort sum | 4,436,134 | 4,409,904 |
| Maximum production function effort | 117,909 | 110,504 |
| All-function cognitive / cyclomatic sums | 759 / 1,930 | 754 / 1,924 |
| All-function effort sum | 8,865,899 | 8,859,060 |
| Mean locality | 98.4338 | 98.4669 |
| Mean observed leverage | 25.4412 | 25.5882 |
| Duplicated lines | 652 | 638 |
| Escape-hatch records | 1 | 0 |
| Production reliability findings | 8 | 8 |
| Production-file `.clone()` / `.cloned()` sites | 92 | 91 |
| Production-file `.to_owned()` sites | 24 | 23 |
| Production-file physical Rust lines | 10,171 | 10,147 |
| All physical Rust lines | 14,607 | 14,596 |
| All Rust/Python/shell lines | 15,313 | 15,302 |

These are modest improvements on an already-refactored baseline. Maximum cognitive
and cyclomatic complexity did not fall. Clone-site counts are syntax counts:
selected desktop actions still require an owned response copy via `to_vec()`;
rejected candidates no longer copy them.

**Two per-module regressions remain:** `resources::system` leverage falls
20 → 10 because discovery no longer consumes that implementation module, and
`model` locality falls 93.25 → 92.5 as explicit test imports expose another consumer.
Discovery locality rises 97 → 100. History and history-summary observed leverage
rise 60 → 70 and 10 → 20; these gains reflect explicit dependency visibility, not
new runtime reuse. The actual new implementation reuse is shared pruning and hashing.
The strict paired no-regression gate therefore does **not** pass every module.
The tracked `quality/regression-baseline.json` and policy were not changed; comparison
with that older baseline still reports 19 regressions/incomparabilities.

All six focused producers agree on each snapshot fingerprint:
`298c2f52debdf8b0` → `d99af29094c1c57f`. Raw artifacts and compact snapshots are
retained locally in `target/review-current/{before,after}/` and
`target/review-current/{before,after}.json`. Instrument SHA-256:
`606229744191af27c6c3652691f61984b9ba30c3a942c5968d9b6b4f15bcc9f2`;
generator 0.1.0, complexity model v2, Halstead v1, architecture risk v4,
observed-reuse-v2 leverage, Rust 1.95.0.

Inventory remains **partial** for the unexpanded `declare_methods!` macro.
Collected dependency references resolve completely; generated inventory is not
therefore complete. The live sibling framework changed during this review;
RQLens rejected changing-input runs, and the final successful six-producer run
was internally consistent. These measurements cover app-daemon, not a controlled
before/after assessment of the sibling changes. No strict completeness claim is made.

## Allocation evidence

Unchanged release `workloads` fixture, 100 iterations, before from a temporary
sibling worktree at `2fdf74c` and after from this tree. Both use the live framework.
The temporary worktree was removed afterward.

| Workload | Allocations/iteration | Allocated bytes/iteration |
| --- | ---: | ---: |
| Empty query | 102,004 → 102,004 | 12,940,634 → 12,932,634 |
| Search query | 20,498 → 19,498 | 1,335,958 → 1,195,558 |
| Warm sampling | 9,093 → 9,093 | 2,181,459 → 2,181,459 |
| Blocked-worker sampling | 9,028 → 9,028 | 2,168,161 → 2,168,161 |

Identity resolution and procfs parsing remain allocation-free in this fixture.
Counts are calling-thread observations; the fixture does not quantify the deferred
nonempty action payloads or live battery/provenance paths. Builds and analysis ran
concurrently, so wall-time measurements are not used to claim a latency improvement.

## Verification and reproduction

- All-feature tests: 32 library tests and 15 session tests pass; five opt-in/helper
  registrations remain ignored. The library subprocess helper also passes.
- All-target/all-feature Clippy with `-D warnings`, formatting, rustdoc, three
  quality-gate Python tests and `git diff --check` pass.
- The separate RQLens `verify` wrapper rejected publication when sibling inputs
  changed again. The checks listed above were subsequently rerun directly and pass;
  no successful `rust_practices.json` publication is claimed for this pass.
- No new coverage, mutation testing or real user-systemd lifetime run was performed.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo doc --locked --all-features --no-deps
python3 -m unittest discover -s scripts -p test_quality_regression.py
# Temporary review snapshot only: do not replace the tracked baseline.
python3 scripts/quality_regression.py \
  --baseline target/review-current/reproduced.json \
  --accept-reason 'Review snapshot only; tracked baseline unchanged'
APP_DAEMON_BENCH_ITERS=100 cargo bench --locked --features benchmarks --bench workloads
```
