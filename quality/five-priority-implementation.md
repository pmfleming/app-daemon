# Five priority improvements

Starting point: `f249edc`. Each step is implemented, validated and committed
separately. Local RQLens 0.1.0, complexity v2 / Halstead v1 / architecture v4,
with unchanged `quality/regression.toml` and policy. Compact paired snapshots
and benchmark/test logs live under `target/quality-five/` (ignored).
`quality/regression-baseline.json` is not replaced.

Measurements are authored-source heuristics, not correctness proofs. Inventory
remains partial for `declare_methods!`; semantic dependency resolution is complete
for collected references. Added helpers contribute to complexity sums, including
the base cyclomatic count of one even for a branchless constructor.

## 1. Defer availability projection during acquisition

`ResourceUsage::unprojected` initializes acquisition without a 48-key map;
`ProcessAggregation` uses it instead of the public default. Projection still occurs
only after target-specific disk metadata is finalized. Public `Default`, missing-field
Serde defaults and JSON shape are unchanged. A new regression checks those contracts
and zero allocations for aggregate construction with the benchmarks feature.

Synthetic release workloads, 100 iterations; counts are per iteration on the
calling thread, not worker allocations or a latency guarantee:

| Workload | Allocations before → after | Bytes before → after |
| --- | ---: | ---: |
| Empty query | 102,004 → 60,704 | 12,932,634 → 7,149,934 |
| Search query | 19,498 → 18,200 | 1,195,558 → 1,013,816 |
| Warm sampling | 9,093 → 5,317 | 2,181,459 → 1,652,755 |
| Blocked-worker sampling | 9,028 → 5,252 | 2,168,161 → 1,639,457 |

Tradeoffs: production cognitive sum stays 549; cyclomatic sum 1,242 → 1,244,
effort 4,409,904 → 4,412,810. Production-file lines grow 30, all Rust lines 60
(including the regression). Locality mean 98.4669 → 98.4559 because the test exposes
another model consumer (model locality 92.5 → 91.75). Leverage, duplicate lines,
escape hatches and clone counts are unchanged. This is an allocation improvement,
not a claim that every static gate improves.

Validation: 33 library and 15 session tests pass, five existing ignored registrations
unchanged; all-target/all-feature Clippy with `-D warnings`, formatting and diff
checks pass. RQLens paired snapshots: `before.json` → `step1.json`.

## 2. Borrow query identities until pagination

Candidates retain borrowed catalog entries/preferences and `Cow` IDs, owning only
unmatched IDs and window groups. Name/category/search fields are borrowed views;
selected rows alone construct `ApplicationIdentity`, copy action payloads and hash
row revisions. Cached sorting borrows catalog IDs rather than copying them. Unmatched
window keywords still appear in the search corpus twice, preserving existing offsets
and acronym scores. The public regression now checks full identity JSON, configured
workspace defaults, unmatched preference handling, row revisions, Unicode metadata,
exact matches, acronyms, terms, pagination equivalence and negative searches.

Against step 1: empty-query allocations 60,704 → 54,704 and bytes 7,149,934 → 6,821,434;
search allocations 18,200 → 8,110 and bytes 1,013,816 → 582,043. Other workload allocation
counts are unchanged. Same release fixture/100 iterations as step 1.

Production cognitive sum stays 549; effort falls 4,412,810 → 4,392,396 and clone sites
91 → 83. Cyclomatic sum rises 1,244 → 1,248 (more small view methods), production-file
lines grow 9 and total Rust lines grow 52 including extended contracts. Architecture,
duplicated lines and escape hatches are unchanged. All-function effort grows because
of the stronger regression. This deliberately prioritizes fewer per-candidate copies,
not hiding complexity through test removal.

Validation: 33 library and 15 session tests, strict all-target/all-feature Clippy,
formatting and diff checks pass. Paired snapshots: `step1.json` → `step2.json`.

## 3. Separate watcher waits, transitions and effects

`EventSources::next` only selects cancellation-safe waits and returns a signal.
`WatchState::react` then performs the selected debounce/recovery effect, outside
that select. `WindowState` owns explicit connected/disconnected/closed states,
snapshot availability and the completed-read timestamp. Catalog invalidation and
periodic repair use the existing backoff owner; repeated events cannot postpone an
already scheduled retry. Initial refresh now also uses the common refresh path,
timestamping each completed read rather than both timestamps after the join.

New tests cover relevant/irrelevant events, connected-but-unavailable snapshots,
disconnection/reconnection/closure, catalog batch priority, resume during recovery,
and pending debounce with another stream/resume already ready. Closed window and
resume streams cannot busy-loop. Existing retry-storm and private-compositor
reconnection tests remain intact.

Watcher orchestration effort falls 110,504 → 32,619; the new transition handler is
25,662, and combined production effort falls 4,392,396 → 4,362,592. Maximum production
effort is now 91,254 (history accumulation). Tradeoffs: production cognitive sum
549 → 555, cyclomatic 1,248 → 1,264, production-file lines +94, total Rust lines +204.
This is explicit, testable state rather than moving one giant function unchanged;
its additional enum dispatch and helpers are not free in static complexity/size.
Mean locality is unchanged; observed leverage 25.5882 → 25.7353 partly reflects new
explicit test dependency visibility. Clone sites fall 83 → 82; duplication and
escape hatches are unchanged. No runtime speedup is claimed for watcher orchestration.

Validation: 35 library and 15 session tests pass, strict all-target/all-feature
Clippy, formatting and diff checks pass. Snapshots: `step2.json` → `step3.json`.
