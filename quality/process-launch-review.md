# Process, launch, and watcher refactor

## Review and changes

- **Launch ownership:** `src/launch/provenance.rs` repeatedly scanned every process
  until ownership stopped growing. It now builds valid parent edges and traverses
  each descendant once, retaining PID/start-time checks, migrated children, and
  zombie rejection. `src/process.rs` shares procfs enumeration, field splitting,
  cgroup reads, and cycle-safe traversal with resource sampling. Launching and
  window identity no longer depend on the resource sampler for cgroup reads.
- **Launch outcomes:** `src/service/action.rs` mixed handoff, placement, and error
  reporting. Outcome handling is now independently testable; successful receipts
  and successful placement survive later failures. Removed forwarding wrappers
  and an always-`None` constructor argument. Window correlation consumes the
  selected client instead of cloning an address and searching the snapshot again;
  baseline addresses, close targets, and display names are borrowed.
- **Sampling:** cgroup paths and GPU engine keys are borrowed during aggregation.
  GPU scratch state is local to aggregation. Network baseline updates now stay
  inside network sampling rather than traveling through a redundant result type.
  Saturating counter accumulation is shared in `src/metrics.rs`.
- **Watcher locality:** reconciliation, intervals, and recovery now live together
  in `src/service/watcher.rs`; recovery's implementation is private. Removed a
  redundant refresh condition that could only be true after the preceding guard.
- **Tests:** consolidated compositor response/command/move state under one lock,
  removing repeated Arc cloning and nested request handling from server startup.
  Replaced all eight wildcard imports with explicit dependencies. Added outcome
  regression coverage and extended ancestry coverage; no tests were removed.

## RQLens comparison

Same `rqlens.toml`, tool binary, and Rust 1.95.0 for both measurements. Raw evidence
is retained locally in `target/review-before/` and `target/review-after/`.
Production functions exclude paths/names containing `test` or `benchmark` and
require a `src/` path; all-code totals remain visible below.

| Signal | Before | After |
| --- | ---: | ---: |
| Production cognitive complexity, sum | 494 | 479 |
| Production cyclomatic complexity, sum | 1,065 | 1,054 |
| Production Halstead effort, sum | 3,924,547.62 | 3,888,164.63 |
| Production function SLOC, sum | 6,028 | 5,967 |
| All-code cognitive complexity, sum | 624 | 604 |
| All-code cyclomatic complexity, sum | 1,573 | 1,567 |
| All-code Halstead effort, sum | 6,919,639.20 | 6,891,544.82 |
| All-code function SLOC, sum | 9,634 | 9,611 |
| Duplicated nonblank lines | 569 | 553 |
| Duplication percentage | 5.02% | 4.88% |
| Escape-hatch occurrences (all wildcard imports) | 8 | 0 |
| `.clone()` call sites, including tests (text count) | 131 | 126 |
| Mean module leverage (observed-reuse model) | 20.00 | 20.63 |
| Service locality | 79.00 | 85.00 |
| Mean module locality | 99.08 | 98.99 |
| Rust physical lines, including tests/benches/new module | 12,245 | 12,244 |

The line reduction is deliberately modest after adding regression coverage.
These are static signals, not measured runtime speedups or allocation totals.
`Provenance::observe_processes` cognitive/cyclomatic/effort changes from
12/8/30,372 to 0/3/21,398. The launch hotspot changes from 16/14/90,995 to
6/8/38,253 **plus** its extracted outcome handler at 7/8/28,693. Extraction does
not remove all decision paths: combined cyclomatic complexity there increases.

Locality is **not** a universal win: resources drops from 86.50 to 81.25 as its
shared process dependency and explicit test dependencies become visible. The
new process module has locality 100 and leverage 60; service leverage improves
60 to 70, but watcher leverage drops 10 to 0 after privatizing recovery. Module
means compare 62 modules before with 63 afterward, not identical populations.

## Verification and limits

- Baseline: 47 unit + 12 integration tests passed. After: **48 unit + 12 integration
  tests passed**, with all features/targets enabled. Three opt-in real-systemd
  tests remain ignored. The debug benchmark target also executed successfully;
  it was not a controlled before/after performance experiment.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` passed.
- `rqlens verify` passed formatting, check, Clippy, tests, doctests, and rustdoc:
  zero error-level failures/diagnostics. Five existing project-practice warnings
  remain: MSRV, contributing guide, code of conduct, security policy, changelog.
- Reliability findings fall 117 to 114, entirely in tests (111 to 108).
  Six benchmark unwrap findings remain classified as production by RQLens.
  No unsafe blocks or lint suppressions were added. Compiler/Clippy checks found
  no unused-code warnings; this is not proof that every public API has consumers.
- All six measurements are explicitly **partial**: existing `usage_fields!`
  item macros in `src/model.rs` and a block-local type in `src/api.rs` limit the
  syntax inventory. Semantic dependency resolution reports 100% for the
  references it collected, not complete macro expansion. No policy-completeness,
  coverage, mutation, security-audit, or real-compositor claim is made.
- Remaining maintenance hotspots include `service::track_resources` (cognitive
  11), catalog watch planning/creation (9 each), and disk traversal/cache/worker
  handling (8 each). Avoid reducing these scores by hiding failure handling or
  weakening their resource bounds. Production allocation profiling is a useful
  follow-up before larger sampler changes.

## Reproduce and identify evidence

```sh
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage; do
  "$RQL" measure "$tool" --config rqlens.toml || exit
done
"$RQL" verify --config rqlens.toml
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Preserve the generated JSON in separate before/after directories before editing;
these commands overwrite `target/quality-current/`. This focused review does not
replace the historical complete baseline or claim its policy gates passed.

- Application base commit: `82ccb28be9594613b84e9412ade05a6b55de14de`.
- Input fingerprints: `8de1e943f42ce6b6` → `b69a7af4e8ba04db`.
- Source digests: `1b74c27567e059ef` → `a15729df039716ce`.
- RQLens checkout: `../rust-quality-lens`, commit
  `c6898928e133c2d5bd21ca4ed29c29e62e1ead4d`; generator version `0.1.0`.
- Executed binary SHA-256:
  `bb0531bb5ca86e1cd0d560cb58ad1efeaaf9e53205d927796b789af53fea8cec`.
- Metric models: standard complexity v2, Halstead v1, architecture risk v4.
