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
