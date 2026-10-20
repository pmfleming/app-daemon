# GPU/history/action refactor review

Fresh paired measurements using the locally built `../rust-quality-lens` checkout
(RQLens 0.1.0, risk model 4, complexity model 2, Halstead model 1), starting at
`85338a3c25a59ad9173f64d2d7c9ec233032e63a`.
Raw evidence and equivalent configurations are retained in
`target/refactor-review/{before,after}/`. Historical `quality-current` evidence
was not reused or overwritten.

## Changes

- **GPU ownership:** `resources::gpu::Sampler` owns identity-keyed counter
  baselines. It moves the provider's engine maps into the next baseline rather
  than cloning every engine name into a flattened PID/start/engine key. Missing
  observations, PID reuse and resume still rebaseline. GPU usage is a cohesive
  value rather than four separately maintained fields in the main sampler.
- **GPU aggregation:** feed deduplicated clients directly into process totals;
  delete the intermediate per-process client maps and single-use memory getters.
  Lowest-PID ownership, per-client maximum merging and device-separated engine
  occupancy are unchanged.
- **History:** aggregate energy one target at a time using borrowed keys, without
  a temporary accumulator hash map. Only emitted target IDs are copied. Retain
  endpoint filtering for finished buckets and overlap filtering for pending ones.
  Use the sorted deque invariant to prune an expired prefix with
  `partition_point`/`drain`, instead of two manual pop loops.
- **Actions:** return the validated `Client`, eliminating a second window lookup
  and its redundant failure path. Borrow verified ownership IDs instead of
  allocating them for comparisons. Separate close dispatch/publication from
  observation polling, retaining cancellation progress, dispatch failures,
  unavailable-compositor handling and timeout behavior.

No wire/schema changes, lint suppressions or unsafe code were introduced. No
public API was removed merely because it lacked an internal production caller.
The deleted helpers were redundant scaffolding, not compiler-proven unreachable
code. Required persistence snapshots, broadcast copies and disk-worker panic
containment remain intentional.

## Measurements and tradeoffs

Production-function scope is `src/`, excluding paths containing `tests`,
`test_provider`, `benchmarks`, or `fixtures`, and names containing `::tests::`.
Architecture means are unweighted over the same 72 measured modules. Physical
line counts cover tracked Rust files, including tests and inline test modules.

| Measurement | Before | After |
| --- | ---: | ---: |
| Production functions | 468 | 466 |
| Production cognitive complexity, sum | 559 | 552 |
| Production cyclomatic complexity, sum | 1,207 | 1,201 |
| Production function Halstead effort, sum | 4,364,054 | 4,346,092 |
| All-function cognitive complexity, sum | 760 | 756 |
| All-function cyclomatic complexity, sum | 1,882 | 1,882 |
| All-function Halstead effort, sum | 8,888,207 | 8,913,828 |
| Mean module locality | 98.594 | 98.542 |
| Mean module leverage | 24.028 | 24.306 |
| GPU module leverage | 20 | 30 |
| Main resources module locality | 90.25 | 87.25 |
| AST / token duplicate groups | 2 / 53 | 2 / 53 |
| Duplicated lines | 638 | 638 |
| Escape-hatch findings | 0 | 0 |
| `.clone()` / `.cloned()` syntax sites, all tracked Rust | 149 | 147 |
| `.to_owned()` syntax sites, all tracked Rust | 38 | 37 |
| Physical Rust lines, production-file filter above | 9,857 | 9,847 |
| Physical Rust lines, all tracked Rust | 14,339 | 14,400 |

The close dispatcher improves from cognitive/cyclomatic **10/7 to 3/3**; its new
polling helper is **7/5**. Their combined effort falls from **57,571 to 48,902**,
but splitting adds one function's baseline cyclomatic point. History pruning
improves from **5/4 to 1/2** without a helper extraction.

**This is not an across-the-board metric win.** GPU encapsulation adds a direct
internal dependency to `resources`, lowering its locality score; its module
implementation is more cohesive, but the measured locality regression remains.
Extra regression coverage increases total lines and all-code effort. Production
lines fall by only 10; total lines rise by 61. Duplicate lines and maximum
production complexity (cognitive 10, cyclomatic 15) are unchanged. No tests were
pruned or measurements excluded to manufacture improvement. Clone-site counts
are source observations, not allocation or throughput benchmarks.

## Verification and limits

- Baseline: 30 library tests and 14 private-session integration tests pass.
- Refactor: `cargo test --locked --all-features` passes 31 library tests and the
  same 14 integration tests. The new energy regression checks finished-only,
  pending-only, combined and excluded targets. GPU coverage additionally checks
  missing-observation recovery alongside existing identity/resume rebaselining,
  memory accounting and multi-device occupancy assertions.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`, formatting
  and `git diff --check` pass.
- RQLens `verify` passes formatting, compilation, Clippy, tests, doctests and
  rustdoc: zero error-level failures. Five existing project warnings remain
  (MSRV, contributing guide, code of conduct, security policy, changelog);
  14 optional/inapplicable checks are skipped.
- Three registered fixtures remain ignored in the ordinary run: the internal
  launch helper (also exercised in its subprocess) and two opt-in systemd lifetime
  fixtures. No live compositor/systemd lifetime run, coverage comparison or
  performance benchmark was performed.

Static evidence is **partial**, not a completeness gate pass: the existing
`declare_methods!` item macro is unexpanded. Semantic dependency resolution is
100% for collected references, not proof of complete source inventory. Reliability
findings rise 131 → 132 because the GPU test sampling helper has an additional
`unwrap`; production panic paths were not added.

Remaining priorities are the ownership traversal (cognitive 10), watcher
orchestration (Halstead effort 168,582), and the broader resource orchestration
boundary. These need behavior-led architectural work rather than moving
functions solely to alter dependency scores.

## Reproduce

```sh
cargo build --manifest-path ../rust-quality-lens/Cargo.toml --locked --bin rqlens
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage; do
  "$RQL" measure "$tool" --config target/refactor-review/after/rqlens.toml || exit
done
"$RQL" verify --config target/refactor-review/after/rqlens.toml
cargo test --locked --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
```

The retained configurations have the same measurement/verification settings as
`rqlens.toml`, with `project_root = "../../.."` and phase-specific output paths.
For a new checkout, use `rqlens.toml` directly or recreate those output-only
configuration copies. A focused run does not populate all producers required by
the separate full-baseline exporter or strict completeness policy.
