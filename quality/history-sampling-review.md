# History/sampling quality review

## Scope and evidence

Measured with `../rust-quality-lens/target/debug/rqlens` (0.1.0), Rust 1.95.0,
architecture-risk v4 and observed-reuse-v2 leverage. This is a fresh before/after
comparison, not a comparison with older reports using different leverage models.
The existing edits in `src/history/recovery_tests.rs` and
`src/resources/background_tests.rs` were included in the baseline and preserved.

Raw artifacts and verification logs are in `target/quality-review/` (untracked):
`before/`, `current/`, and the effective `rqlens.toml`. The six focused producers
have consistent fingerprints within each snapshot:

- Before: `b89e34c09ae625eb`
- After: `537443eb5b84526e`

Both inventories are **partial**: `protocol/methods.rs` contains an unexpanded
`declare_methods!` item macro. The previous unresolved `ResourceAvailability`
impl owner in `projection.rs` is now explicit. Rust-analyzer resolved 100% of
collected dependency references in both runs; that does not establish inventory
completeness. No completeness/policy gate or new full baseline is claimed.
Concurrent sibling-framework edits briefly invalidated measurement runs; those
runs were rejected and rerun. No framework files were changed by this refactor.

## Findings and changes

- **History summary hotspot:** separated validity, confidence, and total
  conversion; reused the observation-validity rule and removed a separate
  totals traversal. Clipping, observed-duration weighting, peaks, zero values,
  units, confidence precedence, and pagination semantics are unchanged.
- **Deep copying in bucket splitting:** `PendingPoint::add` now borrows the
  original resource payload and apportions counters during accumulation. It
  no longer clones strings and the full availability map for every fragment.
  Cumulative `u128` allocation still conserves integer counters. Compute,
  storage, and network averages now reuse the existing rounding helper.
- **Capability ownership:** moved `ResourceAvailability::for_usage` and
  intersection beside the model, rather than making live sampling depend on
  history aggregation. The boolean-only capability value is now `Copy`.
- **Sampling allocation/indirection:** removed the temporary sampled-I/O map
  and the separate previous-process-map rebuild. Each process's baseline is
  updated only after its deltas are computed; exited PIDs are pruned afterward.
- **Operation locality:** centralized running-state publication with the
  registry lock held through notification. Close observation borrows captured
  clients instead of cloning their strings. Cancellation remains terminal.
- **Smaller support surface:** reused the framework JSON reader, removed
  obsolete sampling/forwarding helpers, eliminated the adjacency `expect`, and
  replaced all six reported glob imports with explicit imports. No unused-code
  warnings were found; this is not a claim that every public API is live.

## Measurements

Production excludes dedicated/inline tests, test providers and benchmarks by
path/name convention. All-code figures retain them. Halstead effort sums are
heuristics, not developer hours; no allocation or latency benchmark was run.

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive complexity, sum / max | 574 / 21 | 562 / 11 |
| Production cyclomatic complexity, sum / max | 1,207 / 22 | 1,200 / 15 |
| Production function Halstead effort, sum | 4,576,540 | 4,515,373 |
| `summarize` cognitive / cyclomatic | 21 / 22 | 6 / 8 |
| `summarize` Halstead effort | 103,447 | 42,503 |
| All-function cognitive / cyclomatic sums | 774 / 1,970 | 763 / 1,964 |
| All-function Halstead effort, sum | 9,174,336 | 9,171,396 |
| Mean module locality | 98.5584 | 98.5877 |
| Mean module leverage | 22.9870 | 23.1169 |
| Resource-module locality | 81.25 | 84.25 |
| Source nonblank lines, including tests | 14,540 | 14,527 |
| Production function SLOC | 6,984 | 6,951 |
| Duplicated lines / percentage | 842 / 5.79% | 840 / 5.78% |
| Reported escape hatches (all glob imports) | 6 | 0 |
| Reliability findings, production-labelled | 9 | 8 |

Gains are modest outside the summary hotspot. Model locality decreases
90.25 → 89.50 as it owns the shared capability behavior. Aggregation leverage
falls 30 → 20 after removing an inappropriate consumer; action leverage rises
60 → 70 and method-registry leverage 30 → 40. Maximum production Halstead effort
remains 201,834. The eight remaining production-labelled reliability warnings
are two serialization `expect`s and six opt-in benchmark `unwrap`s.

## Verification

- RQLens `verify`: rustfmt, cargo check, Clippy, tests, doctests, and rustdoc pass;
  zero error-level failures. Five existing project warnings remain: MSRV,
  contributing guide, code of conduct, security policy, and changelog.
- `cargo clippy --locked --all-targets --all-features -- -D warnings
  -W clippy::redundant_clone`: passes without suppressions.
- Default all-target and all-feature tests pass: 76 unit tests plus 17 integration
  tests. The isolated subprocess helper is also exercised by its parent test.
  Three real-systemd/helper integration tests remain explicitly ignored.
- Extended regressions cover `u64::MAX` and one-byte fragment conservation,
  unchanged gauges, invalid energy observations, and process disappearance /
  reappearance without stale CPU or I/O deltas. Existing restart, capability,
  cancellation, placement, and protocol-fixture tests remain intact.
- Coverage, mutation testing, security audits, dependency liveness and live
  compositor/systemd lifetime behavior were not measured in this focused pass.

To repeat the focused measurements with the retained configuration:

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage; do
  "$RQL" measure "$tool" --config target/quality-review/rqlens.toml || exit
done
"$RQL" verify --config target/quality-review/rqlens.toml
```
