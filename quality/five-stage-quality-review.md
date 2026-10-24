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

## Stage 5 — fresh systemd identity observations

Share validated InvocationID/MainPID readers. InvocationID is now a nonzero
16-byte value at the boundary. Both observations remain uncached, and the read
order is unchanged: invocation, PID, procfs identity, confirming PID, confirming
invocation, active state, confirming procfs identity. Recovery deadlines,
concurrency bounds and rejection of historical ExecMainPID remain intact.

Extended the existing private-bus recovery test rather than adding another
registered test. It now covers zero, short and long InvocationIDs; changing
MainPID; and a restart during the confirming PID read with the **same** PID,
alongside inactive units, historical PIDs and changing invocations. The last case
specifically protects read ordering. A deliberate reversal of confirming reads
failed with `invalid identity mode 8` (unexpected recovered owner); the mutation
was immediately reverted and the correct code passed all validation afterward.
This is one targeted mutation experiment, not a general mutation score.

`live_main_process` cognition falls 4→1, cyclomatic 15→12 and effort
31,500→13,436. Production peak cyclomatic complexity falls 15→12; stage-wide
production effort falls 4,337,737→4,326,066. Total production cyclomatic complexity
rises by two because of the two reusable readers. Full all-feature tests, strict
Clippy, formatting and RQLens verification pass. The larger recovery matrix makes
the private-bus suite take about 55 seconds rather than roughly 20 seconds.

## Overall evidence and limits

| Metric | Baseline | Final |
| --- | ---: | ---: |
| Warm sampling allocated bytes/iteration | 1,652,755 | 847,111 |
| Empty query allocated bytes/iteration | 6,821,434 | 6,781,434 |
| Production cognitive sum | 557 | 557 |
| Production cyclomatic sum / max | 1,274 / 15 | 1,281 / 12 |
| Production effort sum / max | 4,320,344 / 67,797 | 4,326,066 / 80,453 |
| All-code cognitive / cyclomatic sums | 765 / 1,972 | 783 / 2,006 |
| All-code effort sum | 9,235,813 | 9,571,062 |
| Mean locality / leverage | 98.425 / 27.286 | 98.425 / 27.286 |
| Duplicated lines | 670 | 651 |
| Escape findings | 0 | 0 |
| Production-file clone sites | 78 | 77 |
| All Rust clone sites | 135 | 137 |
| Rust physical lines | 15,358 | 15,675 |
| Registered tests, including ignored | 61 | 65 |
| Line coverage | 92.6091% | 92.6972% |
| Covered / executable lines | 7,493 / 8,091 | 7,578 / 8,175 |
| Region coverage | 90.6196% | 90.7607% |

These changes prioritize correctness and bounded runtime work. They do **not**
achieve uniform reductions in complexity, size or test volume: four focused
regressions were added, and the active-only sampling guard raises that function's
effort. The unchanged zero-regression comparator therefore still rejects the
increases. No tracked baseline or policy exception was changed to conceal them.
Production labels use the existing gate's naming filter, including its known
`frontend_contract.rs` classification; file-level counts also include inline tests.
Clone-site counts exclude qualified `Arc::clone` and are not allocation counts.

Final validation: 45 unit and 15 private-bus integration tests pass; five ignored
helper/profile/real-systemd cases remain ignored by the parent suites. Formatting,
strict all-target/all-feature Clippy, rustdoc, cargo-machete and the three Python
gate tests pass. RQLens verify has zero error failures, five existing practice
warnings and fourteen skipped optional checks. Line/region coverage does not
establish branch or full mutation coverage, nor real desktop/systemd behavior.

Static evidence: `target/stages-{0,1,2,3,4,5}{.json,/}`. Final coverage:
`target/stages-5-coverage.json`, generated with `cargo llvm-cov --locked
--all-features --json` and LLVM 21.1.8 tools. Baseline coverage reuses
`target/review-2026-after-coverage.json` from the preceding review: its associated
RQLens source digest exactly matches the new stage-0 source digest. Benchmark and
mutation logs are also retained in `target/stages-evidence/`.

All stages use Rust 1.95.0 and the same RQLens binary SHA-256
`423a8b2f8222b9840871efdabc82f29e700a949ba5d83cfd7f8c3c27386f0158`.
Measured input fingerprints: `80101020eceb25ff` → `51458e7e2c86e4de`.
Artifacts describe pre-commit snapshots (committing changes Git provenance).
Final verification and static fingerprints match. No new incomplete-source
exceptions were introduced; the existing unexpanded protocol macro remains
explicitly partial, while semantic dependency identity is complete.
