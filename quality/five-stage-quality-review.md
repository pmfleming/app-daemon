# Five-stage quality follow-up

Baseline: `0b0dc87`. Each stage is independently tested and committed. Scope is
correctness and bounded work first, then allocations and maintainability; raw
metric changes are evidence, not a reason to remove required safety checks.
Tracked quality policy and baseline are not reset.

## Selected stages

1. File-accounting correctness: one open/mapped union and saturating totals.
2. Sampling memory: retain detailed usage only for active application members.
3. History query work: index endpoint selection without losing summary overlaps.
4. Application ranking: bounded top-k selection without owned-ID sort copies.
5. Systemd recovery: reuse fresh observations without weakening identity checks.

## Stage 1 — file accounting

`ProcessAggregation` now owns one file-identity map rather than separate open and
referenced maps. Each identity retains independent open/reference maxima and the
union of temporary classification. This also removes a reduction helper from the
Linux acquisition module. Open-file totals now saturate like the other counters,
instead of panicking in debug builds or wrapping in release on overflow.

Retained the end-to-end resource aggregation test and added an order-independent
matrix covering different device IDs with the same inode, larger mapped sizes,
conflicting temporary labels, and overflowing totals.

Validation: full `cargo test --locked --all-features`, formatting, and strict
all-target/all-feature Clippy pass. RQLens production cognitive/cyclomatic sums
remain 557/1,274; effort falls 4,320,344→4,317,880. Locality/leverage and escape
hatches are unchanged. Production-file lines increase by 7 and total Rust lines
by 62 (55 new test lines): deliberate correctness coverage, not a size win. The
existing sampling benchmark contains no file records, so no file-allocation or
speedup claim is inferred from it.

Raw evidence: `target/stages-0{.json,/}` and `target/stages-1{.json,/}`. Each scratch
snapshot uses the unchanged `quality/regression.toml` and local RQLens binary.
As in the preceding review, authored-syntax evidence is partial only for the
existing `declare_methods!` expansion; semantic dependency identity is complete.

## Stage 2 — active-only process details

Keep CPU baselines and the energy denominator for every observed process, but
reserve/publish detailed usage only for discovered active members. Background
processes no longer occupy the much larger `ProcessUsage` snapshot map. A matrix
switches active roots through one process, the other, no processes and both;
checks snapshot size, CPU continuity, process counts and attributed energy share.
Existing PID-reuse, disappearance and resume regressions remain intact.

The unchanged release workload (1,000 processes, 256 active; 30 iterations)
measures calling-thread bytes/iteration **1,652,755→847,111** for warm sampling,
and **1,639,457→833,813** with blocked disk workers. Allocations fall 5,317→5,308
and 5,252→5,243 respectively. Stage 1 does not affect this file-free fixture.
These are synthetic allocation results, not a latency guarantee or whole-process
peak-memory measurement. Logs: `/tmp/app-stages-bench-{0,2}.log`.

Full all-feature tests passed; after the final reserve change, focused resource
tests, formatting and strict Clippy passed again. RQLens evidence is in
`target/stages-2{.json,/}`. Compared with stage 1, the active-only guard raises
production cognition 557→559, cyclomatic 1,274→1,275 and effort
4,317,880→4,330,536. This explicit branch trades a small static increase for
roughly 49% less allocated sampling memory. Architecture/escape metrics stay
unchanged. Rust lines increase by 51, mostly the new transition regression.

## Stage 3 — indexed history selection

Share a borrowed, binary-seeking iterator across resource summaries, resource
pagination and energy totals. Pagination seeks past both the lower bound and the
validated cursor, then stops at the upper endpoint. Energy totals likewise stop
at their upper endpoint. Summary selection deliberately does **not** stop there:
a later long-duration bucket can overlap the selected window even after an
intervening non-overlapping bucket. No cursor expiry/rewrite checks were removed.

A differential matrix compares public pages with a linear reference, including
anchors outside the window, exhausted cursors, zero-width windows, clamped limits
and late summary-only overlaps. A 4,096-record prefix probe requires at most 14
comparisons before returning record 4,000. This proves indexed prefix selection,
not constant-time whole queries: summaries still visit the selected observations.

Full all-feature tests, formatting and strict Clippy pass. Evidence:
`target/stages-3{.json,/}`. Production cognition stays 559; cyclomatic rises by one
(shared iterator) to 1,276 and effort rises 4,330,536→4,342,605. Architecture,
duplication and escape metrics are unchanged. Rust lines increase by 105 (95 test
lines). This stage prioritizes bounded prefix work and explicit boundary coverage,
not a static-metric or test-volume win.

## Stage 4 — top-k application ranking

Cache each candidate's normalized name once for matching and ordering. Compare
IDs by reference rather than copying owned window-group IDs into cached sort
keys. Partition at the clamped limit, discard the tail, and sort only selected
candidates. Score-descending/name-ascending/ID-ascending ordering is unchanged.
Selection now has a named boundary, separate from page revision and hydration.

A differential full-sort reference covers 1,006 owned IDs, case-folded name ties,
empty/matching/missing queries, category rejection and limits 0, 1, 2, 499, 1,000
and `usize::MAX`. Existing public-page identity/resource/ranking tests still pass.
Full all-feature tests, formatting and strict Clippy pass.

The unchanged release fixture shows empty-query bytes 6,821,434→6,781,434 and
allocations 54,704→54,703. Search allocations fall 8,110→8,098, but bytes rise
582,043→605,163: keeping the normalized-name slot in every candidate costs more
when very few candidates match. Observed median times were 5,104→3,978 µs (empty)
and 515→511 µs (search), but these short synthetic runs are not latency guarantees.
The benchmark uses catalog IDs; the owned-ID path is exercised by the regression,
not inferred from those allocation numbers.

Evidence: `target/stages-4{.json,/}`, `/tmp/app-stages-bench-{0,4}.log`. Production
effort falls 4,342,605→4,337,737; cognition 559→560 and cyclomatic 1,276→1,279
increase with the selection boundary and partition guard. Clone sites fall by
one; architecture/duplication/escape metrics are unchanged. Rust lines rise by 80,
primarily differential regression coverage.
