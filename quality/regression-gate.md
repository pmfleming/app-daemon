# Source-quality regression gate

Run from this repository, with the live sibling framework and locally built RQLens:

```sh
cargo build --manifest-path ../rust-quality-lens/Cargo.toml --locked --bin rqlens
./scripts/check-quality.sh
```

The entrypoint runs the gate's Python contract tests, rustfmt, strict all-target /
all-feature Clippy, all-feature Rust tests and rustdoc, then regenerates six RQLens
producers using `quality/regression.toml`. No network service or live compositor is
required by the ordinary private-session tests; opt-in systemd fixtures remain
ignored. This is a local co-development gate, not a newly configured hosted CI job.

To run only the metric gate (not compilation/tests):

```sh
python3 scripts/quality_regression.py
```

## What blocks

`quality/regression-baseline.json` sets ceilings for both all-function and
production-function cognitive complexity, cyclomatic complexity and Halstead
**sums and maxima**; duplicate lines; escape hatches; production reliability
findings; Rust physical lines; and Rust `.clone()`/`.cloned()`/`.to_owned()` text
sites. Locality and leverage must not fall, both per module and as unweighted
means. Module/metric inventory changes require review so a new module cannot
silently dilute the means. There is no percentage regression allowance, only
1e-6 absolute tolerance for floating-point representation noise.

Production functions are under `src/`, excluding paths containing `tests`,
`test_provider`, `benchmarks`, or `fixtures`, and function names containing
`::tests::`. Production-file LOC uses the same path filter but includes inline
test modules. These are explicitly heuristic scopes, not compiler-verified cfg
classification. Tests/benchmarks remain included in the all-code budgets.

The gate additionally counts **all tracked and non-ignored untracked `.rs`, `.py`
and `.sh` files**, including its own scripts and contract tests. Adding quality
infrastructure is not free in that budget. Markdown, Nix, JavaScript and generated
build outputs are outside this declared line-count scope. Copy-site text counts
may include comments/strings and do not distinguish cheap Arc copies from deep
clones; they are review signals, not runtime allocation measurements.

## Evidence safeguards

- Always regenerate; there is no reuse/stale-artifact bypass.
- Require matching, complete input fingerprints across producers and run hotspots
  again last to detect input drift between producers, including sibling changes.
- Check source files, executable, gate configuration and partial-evidence policy
  have not changed during measurement.
- Match the measured Rust-file inventory to Git's tracked/non-ignored source set.
- Reject missing, negative, non-finite or null metrics; unknown source recovery;
  missing/stale inputs; and incomplete semantic dependency resolution.
- Require compatible instrument binary hash, generator, artifact schemas, risk /
  metric models, config digest and partial-policy digest. Rebuilding RQLens with a
  different binary may require explicit rebaselining even if version strings match.

`regression-policy.json` has one narrowly bounded exception: the existing
`declare_methods!` item macro in `src/protocol/methods.rs`. Its declarations are
covered by compilation and private-bus contract tests, but **expanded inventory
is still unmeasured**. Passing this gate means no regression in the measured
*authored syntax*, not that the separate strict completeness gate passed. New
unsupported patterns or another occurrence fail. The unrelated lexical-owner
warning was removed by spelling the GPU counter impl's owner explicitly; it was
not waived. A reason string cannot bypass missing or unapproved partial evidence.

## Deliberate baseline changes

Default runs never modify the baseline. First inspect each failure, fix avoidable
regressions, and justify the remaining ones in the review. To accept deliberately:

```sh
./scripts/check-quality.sh --accept-reason 'Explain each accepted size/complexity/architecture tradeoff and its validation'
git diff -- quality/regression-baseline.json
# Review and commit the baseline together with the corresponding implementation.
./scripts/check-quality.sh
```

Acceptance requires a nonblank explanation, still regenerates/validates evidence,
and records the old source digest, every accepted regression/incompatibility, the
reason and timestamp. Git retains baseline history. New metric models, module
inventories or instrument builds likewise need explicit review, not silent score
comparisons. The initial baseline intentionally includes the tests and supporting
scripts added in the five-target follow-up; it is not a claim that every metric
improved from the earlier refactor.

Contract tests exercise each metric's direction, per-module regressions hidden by
averages, population changes, instrument/model incompatibility, missing/stale
artifacts, unknown/over-budget partial inventory, incomplete semantic identity,
invalid numbers, source drift, untracked files and explicit baseline acceptance.
