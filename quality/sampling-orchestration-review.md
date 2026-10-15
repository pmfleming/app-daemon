# Sampling and orchestration review — 2026-10-15

## Scope and findings

Reviewed with the locally rebuilt `../rust-quality-lens` (RQLens 0.1.0), using
`rqlens.toml` unchanged. Starting app-daemon commit:
`d0d4b1e63cd6102829b30bb488f36f913499fd27`.

The main opportunities were watcher/operation orchestration, catalog knowledge in
resource sampling, repeated cgroup membership copies, and duplicated counter and
parser logic. This is an incremental improvement, not a large code-size reduction.

- `src/resources/discovery.rs` now owns catalog/ownership-aware discovery and the
  sampling entrypoints. `src/resources.rs` consumes a discovered sample without
  depending directly on catalog or ownership. Resume checks still surround both
  discovery and detailed sampling; the public sampler API is unchanged.
- Cgroup membership is stored once per **path**, not copied per root. Attribution
  still deduplicates cgroup totals and preserves migrated descendants. Shared-PID
  detection uses a set instead of owner counts and traverses each target tree once.
- `src/resources/provider.rs` shares saturating I/O deltas between procfs and
  cgroups. Missing/recovered controllers still establish a baseline rather than
  reporting historical work. Numeric parsers and unordered revision hashing are
  also shared instead of repeated.
- `src/service/action.rs` owns operation execution after admission. It reuses the
  operation's identity and moves the completed launch receipt's fields into the
  result. Cancellation/admission locking and recoverable progress remain intact.
- Watcher batches share their debounce/drain logic; explicit event priority keeps
  failure above rewatch above ordinary invalidation. Removed the redundant catalog
  channel-open flag (the tracker owns its sender).
- Removed the wildcard test import, redundant helper implementations and duplicate
  derive attributes. Narrowed discovery/action helper visibility. No public API or
  registered regression test was deleted to obtain smaller numbers.

## Measurements

Production functions exclude test/benchmark paths and inline `::tests::` functions,
matching the convention in `scripts/quality-baseline.py`. Architecture means cover
all 74 measured modules. These are advisory syntax metrics, not correctness gates.

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive complexity sum | 569 | 563 |
| Production cyclomatic complexity sum | 1,209 | 1,205 |
| Production Halstead effort sum | 4,563,041 | 4,434,720 |
| Largest function effort | 201,834 | 168,582 |
| Mean locality | 98.541 | 98.581 |
| Mean observed leverage | 23.514 | 23.649 |
| Duplicated lines (RQLens) | 757 | 720 |
| Clone records | 63 | 60 |
| Escape-hatch records | 1 | 0 |
| Tracked Rust physical lines, including tests/benches | 14,828 | 14,825 |
| Tracked Rust nonblank lines | 14,016 | 14,010 |

Targeted effort reductions: `execute_owned` 110,733 → 55,874; close observation
91,759 → 57,571; sampling orchestration 88,471 → 56,882. Extracted functions are
included in the aggregate, which improves by 2.8%; this is not just a comparison
of shortened callers. Production function count increases from 464 to 467.
Maximum cognitive/cyclomatic complexity remains **10/15**, respectively.

Locality improves for `resources` (84.25 → 89.5), with a corresponding tradeoff in
`resources::discovery` (100 → 97). Leverage improves for `metrics` (40 → 50) and
`resources::system` (40 → 50), while ownership's direct-consumer score falls
70 → 60 after the dependency is localized. Neither architecture mean changes much.
The line-count reduction is only three physical lines; retained regression
assertions and explicit ownership boundaries account for much of the saved code.

## Verification and limits

Passed:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features --locked -- -D warnings`
- `cargo test --all-features --locked`: 46 library + 16 integration tests passed;
  five registered tests/helpers remain ignored. Live systemd lifetime fixtures
  were not executed.
- `cargo machete --with-metadata --skip-target-dir .`: no unused dependencies.
- RQLens `verify`: zero error-level failures; nine checks passed, fourteen optional
  checks skipped. The five existing project-practice warnings remain: declared
  MSRV, contributing guide, code of conduct, security policy and changelog.

Existing regressions cover controller recovery, PID reuse, migrated ownership,
operation cancellation, close observation, placement and shutdown serialization.
Expanded existing tests cover independent roots sharing one stored cgroup,
deduplicated cgroup totals, and failure precedence within a watcher batch.
Compiler checks do not prove that all externally visible APIs are used; no such
public API was removed speculatively. Reliability findings remain 142 (134 test,
8 classified production, including opt-in benchmark code).

Both static snapshots remain **partial** because the framework's
`declare_methods!` expansion is not inventoried. All collected dependency
references resolve through rust-analyzer, but that does not establish complete
macro-generated inventory. No new unsupported patterns or lint suppressions were
introduced. Coverage, mutation testing and a strict completeness/regression policy
run were not performed; the historical `quality/baseline.json` is not overwritten.

The live sibling framework changed independently during this session. A measurement
interrupted by changed inputs was discarded and rerun; final static artifacts
share one fingerprint. This review changes only app-daemon.

## Allocation experiment

Ran the existing release workloads for 100 iterations against an archived starting
app-daemon tree and the working tree, both using the live sibling framework.
Calling-thread allocated bytes/sample fell by **4,080** in both sampling workloads:
2,152,803 → 2,148,723 warm; 2,139,505 → 2,135,425 with blocked disk workers.
Allocation counts and query allocations were unchanged. The fixture has no shared
cgroups, so it does not quantify the removed per-root cgroup copies.

This was not a controlled timing experiment: warm median was 769 → 793 µs and
blocked-worker median 761 → 833 µs; query-empty median was 9,438 → 11,175 µs.
Concurrent activity and the live dependency prevent attributing these differences
solely to this patch. **No runtime speedup is claimed.**

## Reproduction and raw evidence

Local artifacts: `target/review-before/`, `target/review-after/`; workload logs are
retained there too. These ignored directories are evidence snapshots, not a new
complete policy baseline. Regenerate focused evidence with:

```sh
RQL=../rust-quality-lens/target/campaign-tool-build/debug/rqlens
for metric in hotspots clones escape-hatches reliability locality leverage; do
  "$RQL" measure "$metric" --config rqlens.toml || exit
done
"$RQL" verify --config rqlens.toml
```

Source counts use `git ls-files '*.rs'`, comparing `git show HEAD:<path>` with the
working file. They include fixtures/benchmarks and exclude this Markdown report.
RQLens' nonblank source denominator has a different inventory (13,751 → 13,743).
