# Test-suite reduction

Baseline: `cc6cd09`. Counted with `cargo test --locked --all-features -- --list`.
The goal was approximately 67% of the registered tests, not a permanent test quota.

| Suite | Before | After |
| --- | ---: | ---: |
| Unit tests | 66 | 42 |
| Private-session integration tests | 8 | 7 |
| Opt-in systemd scenarios/helper (ignored) | 3 | 3 |
| **Registered total** | **77** | **52** |
| Runnable total | 74 | 49 |

Removed 25 registrations: **32.5% fewer**, retaining **67.5%**. No new ignores,
feature gates, disabled assertions, or production behavior changes were introduced.
Related cases share fixtures; this is not the old suite hidden behind one runner.

## Selection and retained evidence

| Area | Reduction | Rationale / remaining coverage |
| --- | ---: | --- |
| Catalog | 4 → 1 | Metadata invalidation and D-Bus-only entries are covered through the session API. The integration scenario checks each launch-metadata revision input separately. Catalog precedence, desktop visibility, empty optional fields and unlaunchable-entry rejection remain together. Cgroup naming cases moved into identity resolution. |
| History | 12 → 7 | Weighted statistics, ordering, target-bound cursors and incremental reads share one pagination scenario. First-bucket and two-sided clipping share a boundary scenario. Measured zero remains in persisted availability coverage. Restart/rewrite, pruning/cursor expiry, backward clocks and retention remain. Removed old unsorted-file normalization and legacy/in-memory nonfinite-value fixtures. |
| Hyprland | 3 → 1 | Minimal event/selector boundary validation remains. Snapshot filtering is checked through the reconnect scenario, including invalid and unmapped windows. Dropped exhaustive event-name enumeration. |
| Launch | 6 → 2 | Real private-session handoffs cover success, failure, timeout, backend selection, service/scope arguments and lifetime. D-Bus activation checks a hyphenated name and escaped object path. One subprocess regression combines capped diagnostics, exit status, spawn failure and inherited stderr. Exact unit ownership remains focused. Dropped helper-only backend/validator examples. |
| Protocol | 2 → 1 | Registry and serialized resource-contract assertions share one contract test; assertions retained. |
| Resources | 22 → 18 | Controller fallback/recovery, socket lifecycle/rebaselining, owner reassignment/PID reuse and mixed cgroup/process accounting each share their existing fixture. Resume, GPU, bounded disk work and incomplete-result handling remain focused. |
| Service | 15 → 10 | Removed fixed polling/event-revision assertions and overlapping operation/shutdown checks. Owner-scoped success/failure recovery moved to D-Bus coverage. Admission, terminal cancellation and expiry remain together. Shutdown save ordering, drain behavior and post-shutdown rejection remain. Lock-order and placement-ownership regressions remain focused. Unit uniqueness moved to actual handoffs. |
| Session integration | 8 → 7 | Recovery without a subscription shares the connection-ownership scenario rather than starting another daemon. All previous integration scenario families remain. |

The client and settings tests and all three opt-in systemd registrations are unchanged.
Resource tests still cover PID reuse, missing controllers, resume baselines, socket
attribution, duplicate roots, escaped descendants and bounded asynchronous I/O.
The three recently fixed regressions—settings/resources lock ordering, summary
window overlap and pruned cursor anchors—retain explicit assertions.

## Validation and tradeoffs

- `cargo test --locked --all-features`: **42 unit + 7 integration passed**;
  three real-systemd registrations remain ignored.
- Strict all-target/all-feature Clippy, formatting and `git diff --check`: passed.
- Instrumented all-feature tests also passed. LLVM line coverage: **5231/6455
  (81.04%) → 5110/6341 (80.59%)**; region coverage: **78.57% → 78.00%**.
- Those LLVM totals include inline test code. A supplementary production-prefix
  segment view, excluding dedicated test/benchmark files and inline `cfg(test)`
  module suffixes, gives **4382/5300 (82.68%) → 4370/5300 (82.45%)**. It counts
  nongap, counted segment spans by source line, merging hits on the same line.
  Production prefixes were also compared against the baseline and are unchanged.

This accepts a small measured coverage loss, not a claim of identical coverage.
Removed legacy normalization/nonfinite fixtures, helper-only direct-backend and
unusual unit-name cases, and scheduler-dependent lifecycle paths account for the
remaining gaps. Exact polling schedules and the generic recent-cache eviction
implementation are no longer asserted separately. Branch/mutation coverage and
live compositor/systemd tests were not run; line coverage alone cannot establish
equivalent defect detection. No latency improvement is claimed.

Raw lists, test logs and LLVM exports are retained locally in
`target/test-reduction-review/{before,after}*`; historical RQLens baseline files
were not overwritten. Reproduce the coverage export at each source revision:

```sh
mkdir -p target/test-reduction-review
LLVM_COV=$(command -v llvm-cov) LLVM_PROFDATA=$(command -v llvm-profdata) \
  CARGO_TARGET_DIR=target/test-pruning-coverage \
  cargo llvm-cov --locked --all-features --json \
  --output-path target/test-reduction-review/after.json
cargo test --locked --all-features -- --list
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```
