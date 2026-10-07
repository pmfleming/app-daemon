# Five follow-up quality improvements

Baseline: `9e981e6`. RQLens evidence before changes is retained locally in
`target/five-improvements/before/`; the existing `rqlens.toml` is unchanged.
Each section records a separately validated implementation commit.

## 1. Project launch receipts into operation results

`operation_result` borrows a receipt and copies only the three public wire fields.
Progress reporting no longer clones its process-ownership map or private unit.
Completion uses the same projection (small public-field copies rather than the
previous moves); receipt ownership/lifetime is independent of output construction.
The existing handoff/cancellation test now checks placement, retained provenance,
private-field omission, and independent event/recovered results.

Validation: `cargo test --locked` (60 passed, 3 opt-in systemd tests ignored),
`cargo clippy --locked --all-targets --all-features -- -D warnings`, formatting,
and `git diff --check` passed. No runtime allocation benchmark is claimed.

## 2. Separate observation contracts from Linux reads

`resources/provider.rs` owns the provider traits and raw process, cgroup, disk,
network, GPU, and battery observations. It has no project-module dependencies.
`resources/system.rs` owns `LinuxResourceProvider` and its implementation; 17
Linux helpers are now private. Sampling/attribution state remains in the sampler,
and disk TTL policy lives with the disk cache. Tests consume the contract rather
than depending on the sampler to re-export backend types. No read behavior, PID
identity checks, cache invalidation, or resume boundaries changed.

Validation: 60 tests passed (3 ignored), all-target/all-feature Clippy with denied
warnings, formatting and diff checks passed. Existing cache/PID-reuse, cgroup,
network, background ownership, resume, and blocked-worker tests all ran.
RQLens resource-module locality improves 81.25 → 84.25; its fan-in drops 17 → 13.
The shared contract has leverage 100, locality 96.25 (17 consumers); moving those
edges does not eliminate them. Whole-project means are locality 98.99 → 98.95 and
leverage 20.63 → 20.94 across 63 → 64 modules; measurements remain syntax-partial.

## 3. Separate disk completion from scheduling

`AppDiskCache::read` now orchestrates retention, startup, completion ingestion,
and scheduling. `collect_completed` drains results even with no active targets,
releases capacity before scheduling, ignores retired targets, and preserves the
last completed footprint on failure. Channel bounds, nonblocking request sends,
worker panic containment, and TTL semantics are unchanged.

Two deterministic channel-driven tests cover bounded outstanding work, late
results for retired targets, initial failure TTL/exact-deadline retries, draining
with no targets, and full/disconnected request channels. Existing real-worker
blocking and failed-refresh tests remain.

Validation: 62 tests passed (3 ignored), all-target/all-feature Clippy, formatting
and diff checks passed. RQLens `read` cognitive/cyclomatic/effort: 8/7/45,488 →
3/5/23,622, plus `collect_completed` at 3/3/11,257. The combined cognitive and
effort signals improve; cyclomatic rises by one function's base path.

## 4. Separate catalog planning, installation, and event classification

`catalog_watch::create` wires together a plan, an event classifier, and an owned
all-or-nothing installer. Parent/symlink replacement coverage has its own iterator;
recursive watches still take precedence regardless of root order. A failed watch
installation drops the entire watcher, leaving existing polling/backoff recovery
in control rather than presenting partial coverage as healthy.

Two new tests cover descendant/root/ancestor/unrelated events, access and empty
notifications, multi-path notifications, backend errors, recursive precedence,
and rejection when the second installation fails after a valid first watch.
Existing missing-root/profile-symlink and bounded-backoff tests remain.

Validation: 64 tests passed (3 ignored), all-target/all-feature Clippy, formatting
and diff checks passed. Production functions in `catalog_watch` have combined
cognitive complexity 19 → 15 and effort about 50,271 → 38,337; cyclomatic 19 → 21
includes three extra helper base paths. `create` drops from cognitive 9 to 2,
and `watch_plan` from 9 to 5. No recovery timing or event filtering was relaxed.

## 5. Repair source inventory and audit unused dependencies/APIs

Replaced `usage_fields!` with five explicit, documented structs and moved the
operation-status request type to module scope. A mechanical before/after check
confirmed identical names, types, and order for all 59 generated fields; derives,
visibility, serde defaults, and JSON field names are unchanged. The resource
contract test still checks every serialized leaf. A new compatibility test covers
empty/partial legacy payloads, round trips, availability defaults, and malformed
field rejection.

The seven source producers (hotspots, clones, escape hatches, reliability,
locality, leverage, API health) now report **complete source evidence**, with
zero unsupported syntax patterns and matching complete input fingerprints.
`scripts/check-source-evidence.py` regenerates those artifacts using the existing
configuration and fails on incomplete/missing/mixed-fingerprint evidence. Its
validator was exercised against clean, partial, incomplete-fingerprint, stale,
malformed, and missing-artifact inputs. No source roots, thresholds, features,
policy exceptions, or confidence flags were altered.

`cargo-machete 0.9.2 --with-metadata --skip-target-dir .` found no unused
dependencies. The tool is now included in the Nix development shell; the flake
passed a syntax parse (a full Nix rebuild was not run). The public-function
reference audit found no declaration-only name candidates in project Rust
sources/tests/benches. Low-reference methods were checked against API, daemon,
shutdown, and opt-in lifetime-test callers. This is not proof of every external
consumer: no public API or dependency was removed speculatively. RQLens API health
is a syntactic visibility/documentation inventory, not unused-API proof.

**Remaining blind spots:** compiler-backed type/impl totals remain partial due to
ordinary derive/attribute/expression macros and cfg-dependent ownership. RQLens's
source-expansion and compiler-inventory documentation explicitly says they do
not clear all body-level limitations. The source repair above is not a claim of
complete compiled type counts, coverage, security auditing, or a full
`check --fail-on partial` pass. Type-health now sees 116 declarations versus 111;
the five additional observations were existing macro-generated types, not five
new runtime types. Public documentation findings are also more visible now.

Final validation: **65 tests passed**, 3 opt-in systemd tests ignored; all-feature
benchmark target executed, without claiming a controlled performance comparison.
All-target/all-feature Clippy with `-D warnings`, formatting, diff checks, and
RQLens verification passed. Verification still has the five existing warning-level
project-practice findings and skips optional audits; the dependency audit was
executed separately rather than misrepresenting a skipped verification check.

## Overall comparison and reproduction

Same RQLens binary/models and `rqlens.toml` throughout. Source inventory improves
from partial to complete, so expanded type/public-surface counts are not a
like-for-like architectural regression measure. These totals retain test costs:

| Signal | Baseline `9e981e6` | After all five |
| --- | ---: | ---: |
| Production cognitive complexity | 479 | 473 |
| Production cyclomatic complexity | 1,054 | 1,057 |
| Production Halstead effort | 3,888,164.63 | 3,865,418.37 |
| All-code cognitive / cyclomatic | 604 / 1,567 | 605 / 1,586 |
| All-code Halstead effort | 6,891,544.82 | 7,062,578.71 |
| Resource locality | 81.25 | 84.25 |
| Mean module locality / leverage | 98.99 / 20.63 | 98.95 / 20.94 |
| Duplicated lines / escape-hatch records | 553 / 0 | 553 / 0 |
| Physical Rust LOC including tests and benches | 12,244 | 12,525 |

The five changes are not a universal metric reduction: extraction adds base
paths, explicit contracts add declarations, and regression coverage adds test
code. Duplication percentage falls only because its denominator grows; duplicated
lines are unchanged. Shared contracts increase their own fan-in. These costs are
kept visible rather than hiding types/tests or reducing checks to improve scores.

```sh
python3 scripts/check-source-evidence.py
cargo machete --with-metadata --skip-target-dir .
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
../rust-quality-lens/target/debug/rqlens verify --config rqlens.toml
# Separately inspect the remaining compiled-inventory limitations:
../rust-quality-lens/target/debug/rqlens measure type-health --config rqlens.toml
```

Raw evidence/logs are local under `target/five-improvements/`: `before`, `provider`,
`disk`, `watch`, `pre-inventory`, and `after`. The pre-inventory snapshot separates
the first four changes from the visibility repair. Final source digest:
`304599cf4c023744`; measured pre-commit input fingerprint: `fad1ceed8154b903`.
A subsequent commit changes the Git fingerprint; rerun producers before freshness
checks. The historical `quality/baseline.json` was intentionally not overwritten.
