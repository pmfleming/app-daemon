# Paired resource/history/service review

Measured against `441384be2f9adcca2e1d44f69b5a09a0e1251ba3`, using the local
`../rust-quality-lens` checkout (the directory is named `rust-quality-lens`, not
`rustqualitylens`). No dependencies, public API paths, wire schemas, metric
configuration, exclusions, or tracked policy baselines were changed.

## Changes and findings

- **History:** decode cursor text with borrowed `Cow` fields, retaining escaped
  JSON compatibility; return only the validated timestamp. Use one query clock
  observation. Consume incoming repair points instead of cloning the fallback.
  Keep peak projection/merging with `ResourcePeaks`, rather than a remote impl.
- **Sampling:** reuse the discovered process-child graph instead of rebuilding
  it. Carry owners through propagation, eliminating an impossible missing-owner
  branch and repeated hash lookups. Share saturating I/O accumulation between
  kernel parsing and cgroup aggregation. Keep acquisition in the provider and
  parsers separate, removing redundant forwarding functions. Construct XDG
  fallback paths lazily. Remove unused private `PreviousProcess` Clone/Copy impls.
- **Service:** place pure energy overview projection beside its unit labels;
  reuse label/rate/rounding arithmetic. Remove three formatter unwraps and the
  glob import. Execute prepared actions directly instead of through a forwarding
  function. Use static declared method names and transfer subscription owner
  strings instead of copying them again.
- **Tests:** reuse one blocked-worker setup for snapshot isolation and abandoned
  query admission. Reuse one operation registry for quota, duplicate, ownership,
  cancellation-before-effects and terminal-state checks. Consolidate frontend
  fixture checks; exact registry equality subsumes weaker membership assertions.
  Deserialize fixture values by reference rather than deep-cloning JSON.
- **Additional coverage:** exercise previously uncalled reply fixture producers,
  checking every declared method's envelope/body key against the live registry;
  check cancellation/error examples, energy unit boundaries/non-finite values,
  saturating I/O, and escaped/borrowed cursors. The plain cursor decoder test
  verifies exactly one allocation (the base64 buffer) with the benchmark feature.
- **Flake fixed:** persistence verification exposed a wall-clock-dependent test:
  its one-second sample sometimes crossed a bucket boundary and became dirty
  immediately. Anchor it inside a future bucket instead of assuming clock phase.

No unused dependencies were found. Public APIs were not deleted merely because
this checkout has no internal caller. No runtime speedup is claimed.

## Paired measurements

Production **function** scope excludes test/fixture/provider/benchmark paths,
inline `::tests::` functions, and `src/frontend_contract.rs`. The existing gate's
older naming filter includes that last test-only file; its unmodified results
remain in the scratch snapshots. All-code figures below retain every scope.
Effort is summed Halstead syntax effort, not developer time.

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 559 / 8 | 555 / 8 |
| Production cyclomatic sum / max | 1,280 / 15 | 1,271 / 15 |
| Production effort sum / max | 4,325,652 / 72,594 | 4,277,430 / 67,797 |
| Production functions | 499 | 492 |
| All-code cognitive sum | 764 | 765 |
| All-code cyclomatic sum | 1,975 | 1,972 |
| All-code effort sum | 9,262,990 | 9,235,813 |
| Mean module leverage | 27.0000 | 27.2857 |
| Mean module locality | 98.4464 | 98.4250 |
| Duplicated lines | 692 | 670 |
| Escape-hatch findings | 1 | 0 |
| Rust physical lines | 15,374 | 15,358 |
| Rust/Python/shell physical lines | 16,080 | 16,064 |
| Textual `.clone()` / `.cloned()` sites | 142 | 135 |
| Textual `.to_owned()` sites | 32 | 30 |
| Test/helper function lines | 4,386 | 4,376 |
| Registered tests, including ignored | 63 | 61 |
| Executed parent-suite tests | 58 | 56 |
| Line coverage, all features | 91.0198% | 92.6091% |
| Covered / executable lines | 7,399 / 8,129 | 7,493 / 8,091 |
| Region coverage | 89.8506% | 90.6196% |

Test/helper lines sum RQLens function spans in `tests/`, test-named Rust files,
`frontend_contract.rs`, and inline `::tests::` modules. Counts include neither
benchmark functions nor all fixture data. Five ignored tests remain unchanged.
Clone-site counts are lexical, exclude qualified `Arc::clone`, and are not heap
allocation counts. Production-file coverage excluding fixtures, test support and
benchmarks also improves: **94.6063% → 94.7833%**; much of the overall coverage
increase deliberately exercises exported offline reply producers.

### Explicit tradeoffs / remaining work

This is **not** a uniformly passing regression gate. All-code cognition rises by
one with the additional table-driven checks. Locality falls by 0.0214 points:
Catalog 97→96.25 and model 91→90.25 gain a consumer in the energy projector.
Service locality remains 76. History leverage rises 70→80; reply-definition
leverage rises 40→50. Other module scores are unchanged. This is observed reuse,
not a claim that all architecture goals were achieved.

The largest production cyclomatic hotspot remains live systemd ownership
validation (15); its restart/PID checks should not be deleted to lower a score.
Large history integration tests still dominate all-code effort. RQLens reports
8 production-scope reliability findings both before and after, including optional
benchmark code. No general dead-code absence or mutation adequacy is claimed.

An experimental model/projection relocation was discarded: it did not improve
locality and reduced observed leverage. The existing public projection boundary
and frozen schema were retained. Further locality work needs a broader boundary
review, not another one-use helper extraction.

## Verification and evidence

Passed on final Rust sources:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- Full all-feature tests, including private-D-Bus integration tests
- `cargo doc --locked --all-features --no-deps`
- `cargo machete` and the three Python quality-gate tests
- `rqlens verify --config quality/regression.toml`: zero error failures,
  five existing project-practice warnings, fourteen skipped optional checks

Both coverage runs used:

```sh
LLVM_COV=$(command -v llvm-cov) LLVM_PROFDATA=$(command -v llvm-profdata) \
  cargo llvm-cov --locked --all-features --json --output-path target/review-2026-SIDE-coverage.json
```

Rust 1.95.0 / LLVM 21.1.8; RQLens 0.1.0, architecture model 4, complexity model 2,
Halstead model 1. Instrument SHA-256:
`423a8b2f8222b9840871efdabc82f29e700a949ba5d83cfd7f8c3c27386f0158`.
Input fingerprints: `fdae3cbffa30cef0` → `d315cec12c78f639`.

Raw source artifacts and compact snapshots are retained locally under
`target/review-2026-{before,after}/` and `target/review-2026-{before,after}.json`.
They were generated with `scripts/quality_regression.py --baseline target/...json
--accept-reason ...` to create **scratch comparisons only**, not accept changes to
`quality/regression-baseline.json`. Applying the unchanged comparator to these
snapshots reports exactly the cognition/locality regressions listed above.

Semantic dependency identity is complete in both runs. Authored-syntax evidence
remains explicitly partial for the existing unexpanded `declare_methods!` macro;
there are no missing/stale inputs or new partial exceptions. Coverage is line and
region coverage, not branch or mutation coverage. Ignored real-systemd/helper
fixtures and optional audit/Miri/mutation gates were not executed as full suites.
