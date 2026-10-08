# Ownership and resource refactor review

Measured with the local `../rust-quality-lens/target/debug/rqlens` (0.1.0,
architecture model v4, observed-reuse leverage v2), Rust 1.95.0 / LLVM 21.1.8.
No scoring configuration, exclusions, lint suppressions or dependency changes.
Public Rust APIs and wire formats are retained, including re-exports of the
history summary types at their original paths.

## Changes

- **Ownership:** separate reconciliation from descendant propagation; replace
  deep snapshot/claim copies with immutable `Arc` sharing. Preserve explicit
  anchors, conflicting claims, application boundaries, PID-reuse rejection and
  reparenting. Tests also check snapshot sharing and old-snapshot isolation.
- **Process evidence:** share live-process parsing and age-checked ancestry in
  `src/process.rs`, removing duplicate launch/ownership implementations.
- **Systemd:** uncached proxies replace repeated raw property conversions.
  Recovery still rereads MainPID, InvocationID, active state and process identity;
  it never substitutes historical ExecMainPID.
- **Resource/history model:** share storage/network rate calculation between live
  and historical observations; move summary DTOs into `src/model.rs`, removing
  the model-to-history dependency cycle. Keep aggregation policy in history.
  Extend rate regression assertions to logical I/O, operations and network.
- **Locality/allocation:** share discovered owner strings with `Arc<str>`;
  keep engine-occupancy grouping beside its only consumer instead of coupling
  the sampler to the Linux GPU reader. Traverse overlapping process roots once.
- **Cleanup:** remove redundant parser/rate implementations, unused private
  Clone derives, the private resume forwarding module, the AbortOnDrop forwarding
  alias, and the only glob import. Use explicit owners on cross-module impls.
- **Test reliability:** verification exposed a query/execute revision race in
  the recovered-ownership integration fixture. A deadline-bounded retry handles
  only an explicit stale-revision rejection; accepted actions are never replayed
  and other errors still fail immediately. All existing assertions remain.

## Measurements

Production-function filtering follows `scripts/quality-baseline.py`: exclude
named test/benchmark/provider files and inline `::tests::` functions. All-code
results below keep those scopes visible. Effort is summed function Halstead
syntax effort, not developer time; file/function totals are not mixed.

| Metric | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 538 / 21 | 529 / 10 |
| Production cyclomatic sum / max | 1,162 / 24 | 1,146 / 15 |
| Production Halstead effort sum | 4,377,297 | 4,208,650 |
| Production function count | 444 | 447 |
| All-code cognitive sum / max | 697 / 21 | 694 / 13 |
| All-code cyclomatic sum / max | 1,788 / 35 | 1,777 / 36 |
| All-code Halstead effort sum | 8,210,330 | 8,106,839 |
| RQLens duplicated lines / percentage | 812 / 6.38% | 782 / 6.15% |
| RQLens clone groups | 62 | 62 |
| Escape-hatch findings | 1 | 0 |
| Reliability findings (all scopes) | 144 | 143 |
| Tracked Rust physical lines, including tests/benches | 13,753 | 13,741 |
| Textual `.clone()` calls | 145 | 137 |
| Explicit `Arc::clone` calls | 24 | 30 |
| Mean module locality | 98.7206 | 98.7351 |
| Mean module leverage | 21.7647 | 21.9403 |
| Resource-module locality | 81.25 | 84.25 |
| Process-module leverage | 70 | 90 |
| Metrics-module leverage | 30 | 40 |

The tiny mean architecture gains are not universal improvements: removing the
zero-reach resume facade changes the denominator from 68 modules to 67.
Ownership leverage falls 90→70, history-summary 30→20 and GPU-reader 20→10 as
consumers move to their proper owners. Catalog locality falls 97→96.25 after
its impl identity becomes explicit. Service locality remains 76.
The retry increases the largest test's cyclomatic/effort metrics; all-code peak
Halstead effort rises 209,822→211,726 while production peak falls to 201,834.
Clone-call counts are lexical, not allocation measurements; the important change
is replacing repeated deep copies with shared immutable data. No runtime speedup
or allocation benchmark result is claimed. LOC reduction is deliberately modest.

## Evidence and verification

Raw focused artifacts are retained locally in `target/refactor-before/` and
`target/refactor-after/`; the existing tracked full baseline is not overwritten.
Input fingerprints: `b9ab80190e0f70f5` → `758cac270be0f192`.
Source digests: `0c78daa252f19ee6` → `faae8ab8b38c0204`.
Both use Git HEAD `1abf60910b317a47c66af5b3f9c0414e3628cc55` and the same binary
(SHA-256 `09b72768d645f97886ae4b22ced05553bbcac988fac290745e66f97405e94053`).

Baseline evidence was **partial**: six unresolved lexical impl owners and three
framework-alias dependency identities. Final source producers are complete and
fingerprint-consistent after explicit ownership/import cleanup. Accordingly,
architecture comparisons are advisory, not a complete-baseline regression gate.

Verified on the final source:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo machete
python3 scripts/check-source-evidence.py
../rust-quality-lens/target/debug/rqlens verify --config rqlens.toml
```

59 unit and 14 integration tests pass; three real-systemd/helper tests remain
ignored. The full session suite also passed three consecutive runs after the
fixture fix. RQLens verification has zero error failures and five existing
project-policy warnings (MSRV, contributing guide, code of conduct, security
policy, changelog). Optional gates remain skipped. Coverage, mutation testing,
benchmarks and real compositor/systemd lifetime tests were not measured here;
older artifacts for those producers in `target/quality-current/` are not current
evidence. The large watcher/service orchestrators remain follow-up work.
