# Test pruning: 97 → 65

## Baseline and target

Counted the existing working tree, including the preceding history/sampling
refactor, with `cargo test --locked --all-features -- --list`.

| Suite | Before | After |
| --- | ---: | ---: |
| Unit registrations | 77 | 47 |
| Private-session integration tests | 17 | 15 |
| Opt-in systemd scenarios/helper | 3 | 3 |
| **Registered total** | **97** | **65** |
| Normally runnable registrations | 93 | 61 |

`97 × 0.67 = 64.99`, rounded to **65**. Removed **32 registrations (32.99%)**,
retaining **67.01%**. The four existing ignored registrations are unchanged:
one unit subprocess helper (invoked by its parent), two opt-in real-systemd
scenarios, and their helper. No tests were newly ignored, gated or hidden behind
a runner. This target is a one-time reduction, not a cap on future regressions.

Rust source across `src/`, `tests/`, and `benches/` shrank **15,663 → 14,714 lines**
(**−949**). Comparing the saved starting sources after excluding test modules
and test-only module declarations found no non-test application code changes.
Existing working-tree changes were retained; obsolete test files were removed
rather than leaving unregistered dead test functions.

## Selection and retained coverage

| Area | Before → after | Decision |
| --- | ---: | --- |
| Protocol, events, client, projection | 11 → 2 | Removed duplicate offline-constructor comparisons, direct correlation-wrapper checks, exact Serde error text, permissive/default compatibility cases and legacy partial-payload tests. Frozen registry validation now shares the live all-method dispatch scenario. Resource wire shapes and generated resource fixture values remain checked. Live operation consumers check envelope fields and private-field exclusion; state/gap envelope checks remain focused. |
| History | 12 → 7 | Removed the overlapping endpoint-only scenario. Combined summary pagination and clipping on one window fixture; restart, gaps and clock-backfill invalidation share one persistence lifecycle. Idle/mixed availability now shares the resource persistence/retention fixture. Removed internal cursor-epoch equality assertions, retaining observable cursor validity and expiry checks. |
| Launch and ownership | 11 → 7 | Removed duplicate migrated-helper ownership setup, retaining end-to-end resource attribution and live restart tests. Proc parsing shares a counter/identity fixture, including embedded parentheses and zombie rejection. Exact unit boundaries remain with launch provenance; cached live-identity rejection shares reparenting/PID-reuse coverage. |
| Resources | 21 → 17 | Removed helper-only descendant traversal and channel-bookkeeping tests. Real cgroup files now drive the missing/malformed/controller-recovery sampling scenario instead of duplicating parser and synthetic-counter fixtures. One disk-cache lifecycle covers completion, removed targets, failed refresh retention and retry delay; real blocked workers still verify bounded work without stalled publication. |
| Service | 18 → 10 | Removed private `Arc` identity/lock checks, duplicate placement correlation and launch-progress fixtures, event classification enumeration and fixed polling-state assertions. Live scenarios retain ownership, ambiguity, receipt recovery and reconnection behavior. Watch-plan regressions share one filesystem fixture. Cancellation/admission now also checks that dispatched close requests are not reported as rolled back. |
| Session integration | 17 → 15 | D-Bus activation/action call recording shares the D-Bus-owner placement scenario, removing a duplicate bus/application fixture. Shutdown of a pending handoff shares the existing owned-cancellation lifecycle. |
| Catalog, Hyprland, settings, opt-in systemd | 7 → 7 | Unchanged registrations. |

These are actual fixture/code deletions, not just calls to the old test functions
from fewer `#[test]` functions. Unique assertions retained from consolidated
scenarios exercise the same fixture lifecycle. Numeric, named and special
workspace confirmation now runs through compositor IPC; the mock was corrected
to expose a `name:` selector's actual workspace name.

Still explicit: independent ownership boundaries and conflicting claims;
reparenting, PID reuse and stale systemd invocations; cgroup fallback, GPU/network
accounting and resume baselines; partial/malformed disk reads; history counter
conservation, invalid energy observations, clipping and expired cursors; operation
ownership, cancellation, launch timeout/failure and post-handoff partial success;
settings/resource lock ordering and serialized final-save/shutdown behavior.

## Verification and tradeoffs

- Default all-target tests and all-feature tests pass: **46 unit + 15 session**
  tests; the isolated stderr helper is additionally invoked by its parent.
- All-feature instrumented tests pass.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passes.
- App-package formatting and `git diff --check`: pass. A workspace-wide
  `cargo fmt --all -- --check` encountered concurrent formatting changes in the
  sibling framework's `routing_tests.rs` and local-build `tests/cli.rs`; those
  unrelated files were not edited to make this check pass.
- LLVM line coverage: **7,503/7,999 (93.80%) → 7,009/7,760 (90.32%)**.
  Region coverage: **92.09% → 89.50%**. These totals include inline test code.
- Supplementary production-prefix segment coverage: **6,209/6,448 (96.29%) →
  5,966/6,448 (92.53%)**. This excludes dedicated tests, benchmark/provider files
  and inline test-module suffixes; counted, nongap segment spans are merged by
  source line. It is not a branch-coverage or mutation-equivalence claim.

The coverage loss is accepted and visible, not described as identical coverage.
Most lost production-line hits are offline protocol/event fixture renderers
(165 lines) and the correlation adapter (25 lines). The adapter remains exercised
by the live bridge test, but that subprocess does not propagate coverage output.
Other losses include helper branches and live procfs acquisition paths previously
hit opportunistically by removed service fixtures. Exact compatibility defaults,
exhaustive notification classification and rejected-send bookkeeping no longer
have dedicated assertions. Live sibling dependencies were not frozen for this
comparison. No mutation run, live systemd run or runtime-speedup claim is made.

## Reproduce

Raw before/after lists, logs, LLVM JSON and starting source copies are retained in
`target/test-pruning-current/` (untracked); earlier quality artifacts are unchanged.

```sh
cargo test --locked --all-features -- --list
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --package app-daemon -- --check
LLVM_COV=$(command -v llvm-cov) LLVM_PROFDATA=$(command -v llvm-profdata) \
  CARGO_TARGET_DIR=target/test-pruning-current/coverage \
  cargo llvm-cov --locked --all-features --json \
  --output-path target/test-pruning-current/after-coverage.json
```
