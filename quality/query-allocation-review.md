# Query and sampling allocation review

Baseline: `db98abc`, after the 97 → 65 test reduction. This pass changes production
code and extends existing regression cases; all **65 registrations** remain.

## Changes

1. **Finalize resource availability at the public boundary.** Target-specific
   disk metadata is applied before `ResourceUsage::with_availability`. Root-only
   and target queries each project once, instead of projecting again after
   `complete`. Public default/wire availability remains unchanged.
2. **Represent unsampled files as absent.** `ProcessUsage` holds an optional
   shared file snapshot. Inactive processes no longer allocate an empty `Arc`
   per sample. File availability still requires a successfully observed FD set;
   unavailable data is not confused with a measured empty set.
3. **Remove temporary cgroup storage.** Controller aggregation traverses a
   reusable borrowed iterator instead of allocating a vector of references.
   Missing controllers still preserve valid procfs counters independently.
4. **Avoid unnecessary query work.** Contiguous name/ID matches return before
   constructing metadata/acronym buffers; such matches necessarily satisfy all
   query terms. The fallback uses a cloned, allocation-free token iterator rather
   than a token vector. Sorting caches score/name/ID keys, preserving descending
   score, case-insensitive name and exact-ID tie ordering. The old single-use
   term-matching helper was removed.
5. **Index close observations.** A borrowed `(address, PID)` set replaces a scan
   of every live window for each captured target: O(live + targets), rather than
   O(live × targets), per observation. Remaining IDs retain capture order, and
   reused addresses, duplicate observations, failed dispatch and unavailable
   compositor snapshots keep their previous semantics.

## Synthetic release benchmark

Unchanged `workloads` fixture: 1,000 catalog entries, 200 windows, 1,000 processes,
256 active processes and 64 targets; 100 timed iterations plus warmup and a
separate allocation-counting pass. Final measurements ran without concurrent
agent tests/builds. Counts cover the calling thread, including synthetic-provider
work, not asynchronous disk workers. These are single local before/after runs,
not statistically established latency guarantees.

| Workload | Allocations / iteration before → after | Allocated bytes before → after | Median µs before → after |
| --- | ---: | ---: | ---: |
| Empty query | 235,201 → 164,803 (−29.9%) | 32,104,802 → 21,326,034 | 11,810.9 → 9,618.3 |
| Search query | 240,614 → 168,837 (−29.8%) | 31,580,276 → 21,604,132 | 12,174.8 → 9,147.3 |
| Warm sampling | 13,613 → 9,093 (−33.2%) | 2,770,787 → 2,152,803 | 949.3 → 753.8 |
| Sampling with blocked disk workers | 13,548 → 9,028 (−33.4%) | 2,757,489 → 2,139,505 | 920.8 → 721.3 |
| Identity resolution without cgroups | 0 → 0 | 0 → 0 | 499.7 → 503.6 |

The benchmark measures the combined patch, not isolated gains for each change.
Cached sorting deliberately copies each ID once and can cost more on already
sorted inputs; its benefit is bounded normalization work, not universally fewer
allocations. The close index likewise adds a set allocation, trading that cost
for linear lookup work; it was not independently benchmarked.

## RustQualityLens

Fresh focused artifacts are in `target/quality-next/{before,current}/`, using
`target/quality-next/rqlens.toml`. Fingerprints:
`8526587b4b426b12` → `9e9207a7a518d125`. All six producers and final verification
agree on their respective snapshot. Generator 0.1.0, architecture-risk v4,
observed-reuse-v2 leverage, Rust 1.95.0.

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 562 / 11 | 562 / 10 |
| Production cyclomatic sum / max | 1,200 / 15 | 1,200 / 15 |
| Production function Halstead effort sum | 4,515,373 | 4,505,022 |
| Production function SLOC | 6,951 | 6,935 |
| Close observer cognitive / cyclomatic | 11 / 8 | 10 / 7 |
| Mean locality / leverage | 98.5811 / 23.5135 | 98.5811 / 23.5135 |
| Escape-hatch findings | 0 | 0 |
| Reliability findings, all scopes | 142 | 142 |
| Duplicated lines | 749 | 749 |
| Nonblank source lines, including tests | 13,619 | 13,677 |

This is chiefly an allocation improvement, not a universal metric gain.
Architecture scores and total production decision counts are flat. Early-return
search logic moves decisions into `search_match`; expanded test cases increase
all-function effort 8,792,573 → 8,873,129, cognitive sum 745 → 746 and cyclomatic
sum 1,900 → 1,902. Total source size grows despite the smaller production-function
SLOC. Duplication percentage falls only because its denominator grows.

Inventory remains **partial** because `declare_methods!` is unexpanded. Semantic
identity resolves 100% of collected references, not 100% of possible generated
items. No strict completeness gate is claimed.

## Verification and remaining opportunities

- All-feature tests pass: 46 unit and 15 session tests, plus the subprocess helper
  invoked by its parent. The four ignored registrations are unchanged.
- Extended cases cover name-case/ID sorting ties, mixed metadata/acronym terms,
  negative searches, finalized disk/CPU/energy availability, inactive file-cache
  expiry, and multiple/duplicate/reused close identities.
- All-target/all-feature Clippy with `-D warnings`, app-package rustfmt and diff
  whitespace checks pass. RQLens verification passes check, tests, doctests,
  rustdoc, formatting and Clippy; five existing project-practice warnings remain
  (MSRV, contributing guide, code of conduct, security policy, changelog).
- No new coverage, mutation or real-systemd run was performed in this pass.
  Allocation counts are not a replacement for these forms of verification.

Queries still materialize resource payloads before filtering/pagination, and
public `ResourceUsage::default` still builds its unavailable-metric map. Deferring
payload materialization is a larger, separate opportunity; this patch does not
change that boundary or introduce lazy public wire types.

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage; do
  "$RQL" measure "$tool" --config target/quality-next/rqlens.toml || exit
done
"$RQL" verify --config target/quality-next/rqlens.toml
APP_DAEMON_BENCH_ITERS=100 cargo bench --locked --features benchmarks --bench workloads
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --package app-daemon -- --check
```
