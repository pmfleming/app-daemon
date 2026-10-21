# Launch/sampling refactor review

Paired measurements use the local `../rust-quality-lens/target/debug/rqlens`
and the unchanged `quality/regression.toml` and partial-evidence policy.
The starting checkout was clean. This is a conservative follow-up: production
cognitive complexity was already bounded at 9; large rewrites were not justified.

## Findings and changes

- **History recovery:** `src/history.rs` cloned application keys for every
  restored point and repeatedly acquired the same map entries. Batch insertion
  now acquires entries once per application, retaining ordered insertion,
  duplicate merging and cursor invalidation. Normal recording uses the same path.
- **Launch routing:** `src/launch.rs` now owns desktop/action activation, Exec
  fallback and terminal routing. The service retains window placement and action
  parameter validation. Terminal arguments and verified ownership roots stream
  through iterators instead of temporary vectors. Launch-only status reuses the
  already validated catalog entry. Receipt recording lives with service actions.
- **Counter sampling:** `ProcessUsage::from_counters` checks PID/start-time identity
  once for CPU, I/O and fault deltas, replacing four sampler helpers and repeated
  map lookups. Missing observations still baseline counters rather than bridge gaps.
- **Watcher orchestration:** `Refresh::apply` separates concurrent reconciliation
  from event selection, preserving refresh timestamps and cancellation boundaries.
- **Reuse:** catalog, compositor and target revisions share `metrics::revision`.
  Hash inputs/order and JSON-safe target masking are preserved.
- **GPU/disk workers:** shared clients merge into their first owner without an
  occupied/vacant branch; a regression test covers owner retention and maximum
  counters. Disk dequeueing releases its mutex before work and terminates on
  channel closure. `RefUnwindSafe` replaces the `AssertUnwindSafe` assertion;
  provider panics are still caught and reported as unavailable samples.

No compiler-reported dead code was found. Superseded private helpers/routing
were removed; public APIs and existing tests were retained. No lint suppressions
or exclusions were added to improve the measurements.

## Fresh before/after evidence

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 557 / 9 | 540 / 8 |
| Production cyclomatic sum / max | 1,213 / 15 | 1,199 / 15 |
| Production Halstead effort sum | 4,385,636 | 4,365,912 |
| Production Halstead effort max | 137,418 | 117,909 |
| All-code cognitive / cyclomatic sum | 756 / 1,860 | 739 / 1,847 |
| All-code Halstead effort sum | 8,331,721 | 8,317,248 |
| Mean locality | 98.5224 | 98.5560 |
| Mean leverage | 24.6269 | 24.7761 |
| Rust `.clone()` / `.cloned()` sites, all / production files | 143 / 94 | 141 / 92 |
| Rust lines, including tests | 13,914 | 13,910 |
| Rust/Python/shell lines | 14,620 | 14,616 |
| Duplicated lines | 656 | 656 |
| RQLens escape hatches / production reliability findings | 0 / 8 | 0 / 8 |

Source savings are deliberately modest: 21 lines before adding the 17-line GPU
regression test, net **4**. This is not a claim of substantial repository shrinkage.
Markdown review documentation is not included in the source-line metric.

Tradeoffs remain visible: service locality improves **76 → 79**, metrics leverage
**80 → 100**, catalog locality declines **98.5 → 97.75** from an additional consumer,
and launch leverage declines **30 → 20** as service orchestration stops depending
directly on launch internals. All-code maxima are unchanged.

## Verification and reproducibility

Passed: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets
--all-features -- -D warnings`, `cargo test --locked --all-features`,
`cargo doc --locked --all-features --no-deps`, the three Python regression-gate
tests, and `git diff --check`. The Rust suite passes 24 library tests and 15
session tests; three registered helper/live-systemd tests remain ignored at the
outer harness level. The subprocess helper is exercised by its parent test.
Live systemd lifetime testing and coverage were not run.

Local paired snapshots: `target/refactor-before.json`, `target/refactor-after.json`.
Raw evidence: `target/refactor-before-raw/`, `target/quality-regression/`.
Each snapshot records source/config/tool fingerprints and all module scores.

Reproduce the comparison without accepting a policy baseline:

```sh
python3 scripts/quality_regression.py --baseline target/refactor-before.json
```

This deliberately exits nonzero for the two module-level tradeoffs above.
The tracked historical gate also fails: before this refactor it already had a
different instrument hash/module inventory and production-metric regressions.
`quality/regression-baseline.json` was **not** replaced or waived.

RQLens 0.1.0 uses architecture risk model 4, complexity model 2 and Halstead model
1 here. Semantic dependency identity is complete. Authored-syntax evidence remains
explicitly partial for the existing unexpanded `declare_methods!` macro in
`src/protocol/methods.rs`; these results do not inventory generated method bodies.
Leverage/locality are advisory graph scores, and clone sites are not allocation
counts. The removed unwind assertion was not counted by RQLens's escape inventory.
