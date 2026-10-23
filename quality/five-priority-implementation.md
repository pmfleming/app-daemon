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

## 4. Share history energy accumulation

`WeightedEnergy` owns energy/battery totals, duration-weighted power/attribution/
coverage, saturating sample counts and label merging. Both live fragments and repaired
finished points feed it a borrowed `EnergyObservation`, without copying labels or
resource payloads. Finalization/rounding is shared. Integer counter splitting,
availability intersection, peak merging, zero-duration fallback and overlap endpoint
repair remain separate because their semantics differ.

A regression compares live accumulation with repaired fragments and checks exact
energy/battery totals, weighted averages, mixed labels, missing availability,
saturating sample counts, overlapping backfills and zero-duration legacy points.
Existing integer-conservation and restart/persistence tests still pass.

The two main accumulation functions' effort falls 162,241 → 120,403; including the
new energy helpers and all other production functions, effort falls
4,362,592 → 4,324,098. Maximum production effort becomes 80,318 (operation admission).
Production cognitive sum stays 555; cyclomatic 1,264 → 1,266. Tradeoffs: production-file
lines +51, all Rust lines +119, all-code clone sites +1 in the overlap fixture,
and duplicated lines 638 → 652 (the new observation mappings remain structurally
similar). Production clone sites and escape hatches are unchanged. Mean locality is
unchanged; observed leverage rises 25.7353 → 25.8824 with the new direct aggregate test.
No allocation or latency improvement is claimed for this step.

Validation: 36 library and 15 session tests pass, strict all-target/all-feature
Clippy, formatting and diff checks pass. Snapshots: `step3.json` → `step4.json`.

## 5. Separate prepared actions from registered execution

The service keeps snapshot acquisition and revision validation beside its locks,
preserving settings-before-resources ordering. `PreparedAction` owns the retained
catalog and effective parameters; launch-only workspace preference policy and
outcome projection stay in `action.rs`. `operations.rs` owns public admission,
execution status publication, completion and owned cancellation.

Admission uses the live framework's `OwnedOperations::insert_with`: duplicate IDs
and quotas are checked before a task is created, under the existing mutex. The
oneshot start gate opens only after insertion, and a failed gate cannot execute
work. This removes duplicate admission/insert wrappers and the abort-handle clone
previously needed to clean up failed insertion. The stopping check remains inside
the operation lock; per-application launch serialization still spans execution
and terminal publication. Late running/completion updates cannot revive a claimed
cancelled operation. Progress and cancellation receipt semantics are unchanged.

New regressions cover duplicate rejection before callback start, cancellation of
queued work, owner visibility, and shutdown while a prepared request waits for
admission and still owns its request permit. Existing quota, placement, stale-token,
private-bus ownership and cancellation tests remain intact.

Admission orchestration effort falls 80,318 → 10,747, with preparation, validation,
registry creation and execution measured separately. Total production effort falls
4,324,098 → 4,299,152; cognitive sum 555 → 554; clone sites 82 → 80 (one is the
workspace copy expressed as `&str` conversion, not an eliminated response allocation).
Tradeoffs: cyclomatic sum 1,266 → 1,273; production-file lines +114 (this scope includes
inline registry tests), total Rust lines +150. Locality is unchanged from step 4;
keeping snapshot acquisition in the service avoids adding query/settings dependencies
to the action module. Observed leverage 25.8824 → 26.1765 includes explicit dependency
visibility, not a claim of equivalent runtime reuse. Duplicate and escape-hatch counts
are unchanged.

Validation: 38 library and 15 session tests pass, five ignored registrations unchanged;
strict all-target/all-feature Clippy, formatting, rustdoc, three quality-gate Python
tests and diff checks pass. RQLens `verify` successfully publishes the same fingerprint
as the six final producers: nine checks pass, zero error failures, five existing
project-practice warnings and 14 skipped checks. Its all-target test invocation also
runs the synthetic workload harness (`APP_DAEMON_BENCH_ITERS=20`); the release allocation
comparison below is a separate 100-iteration run. No new coverage, mutation test or
real user-systemd lifetime run is claimed.

## Overall result and remaining gates

| Metric | Start | After five steps |
| --- | ---: | ---: |
| Production cognitive sum / max | 549 / 8 | 554 / 8 |
| Production cyclomatic sum / max | 1,242 / 15 | 1,273 / 15 |
| Production function effort sum | 4,409,904 | 4,299,152 |
| Maximum production function effort | 110,504 | 72,594 |
| Mean locality / observed leverage | 98.4669 / 25.5882 | 98.4559 / 26.1765 |
| Production-file clone sites | 91 | 80 |
| Production-file `to_owned` sites | 23 | 22 |
| Duplicated lines | 638 | 652 |
| Escape-hatch records | 0 | 0 |
| Production reliability findings (includes benchmark advisories) | 8 | 8 |
| Production-file physical lines | 10,147 | 10,445 |
| All physical Rust lines | 14,596 | 15,181 |
| All-function effort sum | 8,859,060 | 9,142,720 |

The priorities deliver substantial allocation reductions and smaller effort hotspots,
not universal static-metric improvements. Added state types, borrowed views and
regressions increase size and decision totals; no tests or safety checks were deleted
to offset that growth. The strict no-regression gate still fails; the tracked baseline
and policy remain untouched. The only per-module locality decline across the five
steps is model 92.5 → 91.75 from explicit test dependency visibility in step 1.

Final release allocation counts reproduce step 2: empty query 54,704 (−46.4% versus
start), search 8,110 (−58.4%), warm sampling 5,317 (−41.5%) and blocked-worker sampling
5,252. Identity and procfs-parser workloads remain allocation-free. Counts are
fixture-specific; no statistical latency improvement is asserted.

Snapshot fingerprints, in order: `0f14581fcb338211`, `79934881af63c361`,
`7a20807c22ed5e88`, `16ba808c6da3b0a7`, `1a083892d8f69bee`, `2df4231bb64da370`.
Each snapshot is internally consistent; commits change the Git fingerprint, so these
are evidence for the measured pre-commit source snapshots, not freshness claims for
subsequent HEADs. Producer artifacts for each completed step are saved in
`target/quality-five/stepN/`; compact snapshots and logs are adjacent.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo doc --locked --all-features --no-deps
python3 -m unittest discover -s scripts -p test_quality_regression.py
# Review artifact only; never silently accept the tracked regression baseline.
python3 scripts/quality_regression.py --baseline target/quality-five/reproduced.json \
  --accept-reason 'Review snapshot only; tracked baseline unchanged'
APP_DAEMON_BENCH_ITERS=20 ../rust-quality-lens/target/debug/rqlens verify --config quality/regression.toml
APP_DAEMON_BENCH_ITERS=100 cargo bench --locked --features benchmarks --bench workloads
```
