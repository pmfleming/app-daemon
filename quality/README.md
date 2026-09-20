# RustQualityLens baseline

For the newer focused hotspot/allocation refactor and its before/after metrics,
see [refactor-review.md](refactor-review.md). It does not replace this full baseline.
The subsequent [test-suite reduction](test-reduction.md) records the 77 → 52 test
count, retained regression coverage, and coverage tradeoffs.

`baseline.json` is a compact, tracked snapshot of the step-5 measurements.
It preserves generator/model versions, source/config/toolchain/Git fingerprints,
all-code and production-function complexity, per-module locality **and leverage**,
coverage, verification warnings, and policy results. It is a review artifact,
not the directory-format input expected by `rqlens check --baseline`.

The raw snapshot for that command is retained locally in `target/quality-baseline/`.
Fresh measurements now go to `target/quality-current/`, avoiding stale artifacts
left over from earlier reviews in `target/analysis/`. Raw artifacts contain
absolute paths and generated evidence and are intentionally not committed.

## Reproduce

From the repository root, with the local RQLens binary built:

```sh
export LLVM_COV=$(command -v llvm-cov)
export LLVM_PROFDATA=$(command -v llvm-profdata)
RQL=../rust-quality-lens/target/debug/rqlens
for tool in hotspots clones escape-hatches reliability locality leverage module-cohesion \
            architecture-rules test-quality api-health semantic-api type-health; do
  "$RQL" measure "$tool" --config rqlens.toml || exit
done
"$RQL" measure coverage --config rqlens.toml &&
"$RQL" verify --config rqlens.toml &&
"$RQL" measure correctness-run --config rqlens.toml &&
"$RQL" measure function-risk --config rqlens.toml &&
"$RQL" measure map --config rqlens.toml &&
"$RQL" check --config rqlens.toml --fail-on partial --fail-on test-failure --fail-on practice-failure &&
python3 scripts/quality-baseline.py
```

LLVM tools must match rustc's LLVM version (21.1.8 for this run). Integration
coverage requires `dbus-daemon`, `sh`, and `sleep`, provided by the Nix dev shell.
Verification is locked to Cargo.lock. Benchmarks are intentionally not enabled
in this coverage run; their feature build/tests were checked separately.

The exporter rejects incomplete or mixed-fingerprint artifacts and failed gates.
All producers required by the strict completeness check must be run: a partial
focused run is useful for exploration but is not enough for this baseline.
Semantic-API extraction is explicitly disabled by the existing configuration;
a complete status there does not mean a semantic API audit was executed.

After reviewing a new baseline, retain its full raw artifacts if desired:

```sh
mkdir -p target/quality-baseline
cp target/quality-current/*.json target/quality-baseline/
```

To compare future changes, regenerate current measurements **without replacing**
that directory, then use `rqlens check --baseline target/quality-baseline
--fail-on regression --max-regression 5`. A new commit changes the input Git
fingerprint, so regenerate current evidence before checking freshness. The
tracked baseline intentionally describes the measured source/config snapshot;
its Git HEAD identifies the preceding application commit, not its own commit.

## Interpretation and remaining risks

The production-function view excludes dedicated tests, inline `::tests::`
functions, providers, and benchmark files by naming/path convention. It does not
hide them: the all-code view includes everything, including the new integration
bootstrap hotspot (score 153.78, cyclomatic 30). That fixture and the long UWSM
integration scenario are now the largest test-maintenance hotspots.

Mean module leverage is effectively flat (67.10 → 67.08), not a universal gain.
Query locality/leverage improved (97/55 → 100/58), while resource locality dipped
slightly as new consumers increased fan-in. Added tests/benchmarks also increased
all-code complexity and duplication. See `docs/followup.md` for the comparison.

All requested completeness, test-failure, and error-level practice gates pass.
Five existing project-practice warnings remain (MSRV, contributing guide, code
of conduct, security policy, changelog). Six warning-level unwrap findings are
in opt-in benchmark code; no unsafe/lint-suppression escape hatches were added.
The default aggregate threshold is exceeded by `resources` (601.21) and `service`
(622.86) against 600. Threshold enforcement was **not** enabled or waived; these
are follow-up architectural warnings, not passing threshold checks. Branch and
mutation coverage, security audits, and live compositor/systemd integration were
not measured. The synthetic benchmark baseline is separate in `benchmarks/`.
