# RQLens-guided refactor review

## Scope and provenance

Compared application commits `64e2749` and `69f7d3d` using the local
`../rust-quality-lens/target/debug/rqlens` (0.1.0, architecture model v4,
complexity model v2). Tool checkout: `d23a6e72ed4520f192f9c14edb3de0401e0311d0`.
Rust/Cargo: 1.95.0; LLVM: 21.1.8. The configuration, exclusions, tool, and
measurement scope were not changed to improve the scores.

Raw local snapshots are in `target/refactor-before/` and
`target/refactor-after/`. Input fingerprints are `83bfc6e609fe1e16` and
`61d8ce8bffaff679`; source digests are `3e894fc19ad8df4e` and
`1268ab7810d5ec71`. Final focused artifacts and verification share a fingerprint.
This document records those source commits, not its own later documentation commit.
The older full `quality/baseline.json` is intentionally not replaced by this
focused measurement run.

Production-function statistics include app-daemon's `src/` functions, excluding
test/provider/benchmark paths and inline `::tests::` functions. Production-module
statistics exclude dedicated test/provider/benchmark modules (inline tests remain
part of their containing modules). All-code statistics below keep the tests visible.

## Results

| Production function metric | Before | After |
| --- | ---: | ---: |
| Function count | 406 | 403 |
| Cognitive complexity, sum | 476 | 455 |
| Cognitive complexity, maximum | 20 | 11 |
| Cyclomatic complexity, sum | 1,010 | 998 |
| Cyclomatic complexity, maximum | 17 | 10 |
| Hotspot score, sum | 3,921.05 | 3,750.49 |
| Hotspot score, maximum | 148.01 | 68.71 |

RQLens does **not** emit a calibrated effort metric in this version. Its hotspot
scores are maintenance-pressure heuristics, not measured developer hours or a
substitute effort metric.

| Scope / metric | Before | After |
| --- | ---: | ---: |
| Production modules: mean locality | 99.602 | 99.672 |
| Production modules: mean leverage | 70.656 | 71.141 |
| All app modules: mean locality | 99.588 | 99.625 |
| All app modules: mean leverage | 66.933 | 66.842 |
| All app functions: cognitive sum / maximum | 580 / 20 | 558 / 14 |
| All app functions: cyclomatic sum / maximum | 1,483 / 35 | 1,475 / 35 |
| All app functions: maximum hotspot | 176.74 | 176.74 |
| RQLens duplicate lines (includes tests/vendor) | 699 | 618 |
| RQLens duplication percentage | 5.98% | 5.29% |
| RQLens source nonblank lines (includes tests/vendor) | 11,698 | 11,683 |
| Tracked owned Rust physical lines, including tests/benches | 11,771 | 11,759 |
| Owned escape-hatch occurrences | 15 | 7 |
| Normal-runtime panic-path findings | 1 | 0 |

Architecture gains are modest, not universal: all-code leverage declined slightly.
Explicit test imports improve dependency visibility rather than hiding those edges.
The escape-hatch reduction is eight test glob imports; unsafe and lint-suppression
counts were already zero and remain zero. Six existing opt-in benchmark `unwrap`
findings remain; test assertions were not weakened to lower reliability counts.

Semantic identity resolved 433/434 references before and 477/478 after. Both runs
retain one explicitly reported syntax fallback, despite complete producer evidence.
These are heuristic comparisons, not claims of complete compiler-resolved topology.

## Changes

- **Discovery:** separated cgroup discovery, remembered PID validation, and root
  selection; consume remembered maps instead of repeatedly cloning application
  keys. Preserve PID/start-time checks, reassignment, nested ownership boundaries,
  launch-only exclusions, and cycle-safe root selection. `discover`'s hotspot
  score fell from 148.01 to 32.76.
- **Identity:** return borrowed catalog IDs, borrow unescaped systemd unit names,
  avoid allocating suffix strings during catalog scans, and use `Cow` only when
  synthesizing an unmatched window-group ID. Cgroup matching's hotspot fell from
  91.56 to 37.18.
- **History:** one metric descriptor table drives registration and accumulation;
  fixed accumulators replace repeated string-map lookups and the registration
  `expect`. Reuse peak merging for live samples and restored history. Preserve
  clipping, capability gaps, invalid-value handling, and cursor behavior.
  `summarize`'s hotspot fell from 108.08 to 21.67.
- **Launch/service:** put exact-unit ownership in `LaunchReceipt`, removing the
  action layer's direct resource dependency; move receipt fields and terminal
  operation IDs instead of cloning them. Action locality improved 97 to 100 and
  leverage 57.5 to 63.0. Collapse redundant focus/close/address adapters.
- **Cleanup:** one category/workspace table validates settings; consolidate disk
  worker handling; flatten watcher filtering. Remove the unused event/cancellation
  adapters, backend name/scope accessors, and ownerless execute/dispatch wrappers.
  In-tree callers now use the explicit owner-aware APIs.

CLI, D-Bus JSON shapes, and persisted schemas are unchanged. Rust convenience APIs
were deliberately removed (`watch_events`, `cancel_operation`, `execute`,
`dispatch`, backend `name`/`scope`, and `CATEGORIES`); this is not a promise of
source compatibility for unknown external Rust library consumers. No consumers
were found in the checked sibling Rust projects. No vendor/framework source was
modified and no macros or lint suppressions were added to conceal complexity.

## Allocation and timing checks

Existing release workloads, 100 iterations, run against both source revisions.
Three additional alternating before/after runs checked the initially slower final
run against comparable host load. Medians of those three median times (microseconds):

| Workload | Before | After | Allocations per iteration, before → after |
| --- | ---: | ---: | ---: |
| Query, empty | 1,034.097 | 1,030.491 | 22,801 → 22,801 |
| Query, search | 1,563.450 | 1,502.806 | 28,214 → 28,214 |
| Identity, 200 windows without cgroups | 712.004 | 681.988 | **200 → 0** |
| Sampling, warm | 504.696 | 507.781 | 2,742 → 2,742 |
| Sampling, blocked disk workers | 487.734 | 486.051 | 2,677 → 2,677 |

Timing samples are small and host-sensitive; no general speedup is claimed.
Identity allocation bytes fell from 3,200 to zero. Other workload allocation counts
were unchanged. Allocation counting covers the calling thread, not disk workers.
The discovery/history refactors are not isolated by these existing workloads.
Original and alternating-run JSON are retained in the local snapshot directories.

## Validation and remaining work

- `cargo test --locked --all-features`: **66 unit + 8 integration tests passed**;
  the two real-systemd scenarios and their internal helper remain ignored.
- Strict all-target/all-feature Clippy, formatting, and diff checks passed.
- `rqlens verify`: 0 error failures; the same 5 project warnings remain (MSRV,
  contributing guide, code of conduct, security policy, changelog).
- Regression coverage includes borrowed identities, escaped/invalid cgroup unit
  names, exact launch-unit ownership, remembered window reassignment, metric-key
  registration, invalid peaks, and the earlier history/deadlock regressions.
- Coverage, mutation testing, security audits, and live compositor/systemd tests
  were not rerun by this focused review. Optional skipped gates are not passes.

The largest remaining production hotspot is the bounded directory walker (68.71),
followed by operation admission/execution (65.35) and resource tracking (63.50).
The private-session fixture remains the all-code maximum (176.74). Those are
follow-up targets, not reasons to split code mechanically or weaken safeguards.

Reproduce the focused measurements from the repository root:

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage module-cohesion; do
  "$RQL" measure "$tool" --config rqlens.toml || exit
done
"$RQL" verify --config rqlens.toml
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
APP_DAEMON_BENCH_ITERS=100 cargo bench --locked --features benchmarks --bench workloads
```

Copy the focused JSON outputs from `target/quality-current/` before changing source
to retain a comparable baseline. Do not pass a focused snapshot off as the complete
baseline required by `scripts/quality-baseline.py`.
