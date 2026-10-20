# Five-target follow-up

Starting point: `adc9558` (checkpoint of the preceding GPU/history/action refactor).
Each numbered step has its own implementation commit. Raw measurements live in
`target/next-five/`; this review does not replace the full historical baseline.

## 1. Stream GPU usage into the snapshot

`gpu::Sampler::sample` now yields usage records instead of allocating another
PID-to-usage map. The main sampler consumes the entire frame into the process
snapshot; provider reads still occur before the per-process reads. Missing
observations, PID reuse and resume continue to invalidate counter baselines.
No new architectural abstraction or dependency was introduced.

The opt-in `gpu_frame_allocations` test compares streaming against collecting the
same iterator. Inputs (64 processes, two clients each) are prepared outside the
measurement. Before this change the eager implementation measured 455 allocations
and 43,244 bytes; after it, streaming measures 454 allocations and 32,860 bytes
(**24.0% fewer allocated bytes**). The collected comparison still reproduces
455 / 43,244. These are synthetic allocations, not GPU-driver latency evidence.

Verification: all-feature library suite (32 passed, one internal helper ignored),
all-target/all-feature strict Clippy, formatting and diff checks. The provider
fixture now supplies GPU counters so cache tests cover publication through the
real sampler, PID reuse, resume, exit and inactive applications. The fixture's
counter copies are test-only; production still moves counter maps/keys.

Initial RQLens architecture attempts were rejected because the live sibling
`daemon-framework` inputs changed during extraction. No mixed-input architecture
result is accepted as current evidence; final measurements must be regenerated
against stable inputs. Source-size reduction is not claimed for this step:
allocation and integration regressions add test code.

## 2. Centralize watcher reconciliation

Event branches now select a refresh scope instead of repeating I/O and timestamp
updates. One `refresh_if` implementation is shared by catalog and window reads;
resume still refreshes both concurrently, and unrequested futures are never
polled. Catalog installation failures share one diagnostic/backoff path. Rebuild
uses the same initial state as first installation, dropping the old watcher.
The healthy catalog timer still repairs its watch before a single catalog scan.

`track_state` Halstead effort falls **168,582 → 137,418 (18.5%)**, with cognitive /
cyclomatic unchanged at **2 / 3**. The unusually low complexity of this function
reflects RQLens's syntax view of `select!`, not proof that its event logic is simple.
The helper and tests add lines; no total-size reduction is claimed here.

Verification: all-feature library and private-session integration suites pass,
including compositor failure/reconnect and catalog replacement scenarios; strict
Clippy, formatting and diff checks pass. Two new deterministic tests cover capped
retry delays, reset on success/rebuild, skipped/healthy install attempts,
healthy/degraded polling intervals, and conditional refresh timestamps (including
an unrequested pending future that must not be polled).

## 3. Simplify ownership eligibility and propagation

`Process::live_parent` now supplies one PID-reuse/ancestry rule to child-map
construction, retained-lineage checks and root eligibility. Parent-owner lookup
is separate from queue traversal; eligible child identities are filtered before
propagation. Breadth-first ordering, explicit-anchor precedence, conflict markers
and the visited set remain intact.

This also fixes a demonstrated bug: a retained child whose parent PID was reused
could be incorrectly excluded from traversal roots, preventing its new descendants
from inheriting the retained owner. The new regression **fails on the step-2
commit** in a temporary worktree and passes with this fix. It also exercises a
same-start ancestry cycle and preservation of an explicit conflicting anchor.

`Snapshot::inherit` cognitive / cyclomatic complexity falls **10 / 7 → 7 / 6**;
its effort falls **44,267 → 39,488**. The new parent-owner helper costs 4,702 effort,
while shared parent validation reduces `can_retain` effort **23,590 → 18,962**.
These helper costs remain included in aggregate measurements.

Verification: all-feature library and private-session integration suites, strict
Clippy, formatting and diff checks pass. Existing tests retain reparenting, catalog
removal, migrated-scope boundaries, explicit anchors and conflicting claims.

## 4. Audit and narrow test-oriented interfaces

Searched 1,050 discovered sibling Rust files and 27 sibling Cargo manifests,
excluding build outputs, in addition to this repository's production, test and
benchmark callers. No sibling Rust dependency/import or call to these helpers was
found. The only sibling `app-daemon` matches were framework build-policy fixtures
and project names, not Rust API consumers. This is a local-consumer audit, not a
claim about unknown external users.

- `ResourceSampler::sample_for_targets`: restricted to the resources module and
  compiled only for tests or the explicit `benchmarks` feature. Production uses
  catalog/ownership-aware `sample_for_applications` instead of this fixture path.
- `ResourceSnapshot::usage_for_roots`: private and test-only. Application paths
  retain ownership-filtered `usage_for_target` / `usage_for_application`.
- Removed `HistoryStore::query`, a redundant public wrapper. All its test callers
  now exercise `query_window` directly, passing `None` for the end of the window.
  No cursor, persistence, shutdown or resource-history test was removed.

**Rust source compatibility:** these three formerly public helper interfaces are
no longer exported. Any unobserved external source consumer must migrate history
calls to `query_window(id, since, None, cursor, limit)`, resource queries to the
target-aware API, or sampling to `ApplicationService`. IPC/JSON interfaces and
other public Rust entrypoints are unchanged. This deliberate narrowing is not
represented as a semver-compatible public API change.

Verification: default all-target compilation, all-feature tests and strict Clippy,
formatting, diff checks and all-feature rustdoc pass. Generated public docs were
checked to contain none of the three removed/restricted methods. Feature-enabled
benchmark callers still compile. Added cfg guards affect compiled surface; RQLens
pre-cfg inventories may still count the test-only methods.

## 5. Enforce reviewed regression budgets

`./scripts/check-quality.sh` runs eight Python gate-contract tests, rustfmt,
strict all-target/all-feature Clippy, all-feature tests, rustdoc and a fresh
RQLens comparison. The checked-in baseline tracks all/production complexity and
effort sums/maxima, per-module and mean locality/leverage, duplicate lines,
copy-call text sites, escape hatches, production reliability findings and separate
Rust / Rust+Python+shell physical-line budgets. The scripts count their own cost.

The gate rejects missing, inconsistent, stale, non-finite or negative metrics,
unmeasured files, incomplete semantic resolution, new unsupported patterns and
instrument/model/config/policy changes. It checks source and tool stability and
runs hotspots again last to detect mixed producer inputs. Default runs cannot
rewrite the baseline. `--accept-reason` explicitly records the justification and
every accepted regression, and still cannot bypass evidence validity checks.
A real run rejected a one-line increase in the gate's own tests without rewriting
the baseline; the reviewed initialization then explicitly accepted that negative-
metric test. See [regression-gate.md](regression-gate.md) for usage and scope.

Fully qualifying the GPU counter impl owner removes its lexical-identity warning.
Only the pre-existing method-declaration macro remains a narrowly bounded,
documented partial-inventory exception. This is an authored-source regression
gate, **not** a claim that expanded inventory, coverage, mutation or live-systemd
gates passed. The earlier sibling-input churn has stopped; final artifacts have
matching input fingerprints and complete collected-reference identity resolution.

### Aggregate result and remaining tradeoffs

The starting and final raw runs use the same RQLens metric models. The final gate
config additionally enables all features for verification and fingerprints
`test_support`; its config digest differs from the exploratory starting config.
These are descriptive authored-source comparisons, not a compatible-config strict
RQLens regression-policy pass. The permanent gate starts from the explicit reviewed
final baseline and requires that config/model to remain comparable in future runs.

| Measurement | Start (`adc9558`) | Final |
| --- | ---: | ---: |
| Production cognitive sum / max | 552 / 10 | 553 / 9 |
| Production cyclomatic sum / max | 1,201 / 15 | 1,206 / 15 |
| Production function effort sum | 4,346,092 | 4,328,336 |
| Production maximum function effort | 168,582 | 137,418 |
| All-function cognitive / cyclomatic sums | 756 / 1,882 | 764 / 1,895 |
| All-function effort sum | 8,913,828 | 9,093,872 |
| Measured modules | 72 | 73 |
| Mean locality / leverage | 98.542 / 24.306 | 98.562 / 24.110 |
| Duplicated Rust lines | 638 | 678 |
| Production-file clone/cloned sites | 99 | 99 |
| All-Rust clone/cloned sites | 147 | 148 |
| Escape-hatch findings | 0 | 0 |
| Production-file Rust lines | 9,847 | 9,888 |
| All Rust lines | 14,400 | 14,642 |
| All `.rs` / `.py` / `.sh` lines | 14,742 | 15,400 |

Locality is unchanged for every existing module; the new watcher test module
scores 100. Watcher leverage rises 0 → 10 with its new test consumer; every other
existing module's leverage is unchanged. The new test module has leverage 0,
so the mean falls despite that local gain. This is not a universal architecture
improvement or a net LOC reduction. Added tests, explicit scheduling and regression
infrastructure increase size, duplication and some complexity totals. Those costs
are retained and budgeted instead of being hidden through test deletion or broad
exclusions. The previous main-resource locality regression was not further worsened.

Final validation: **35 library tests + 14 private-session integration tests pass**,
plus the internal launch helper exercised in its subprocess; eight gate-contract
tests, strict Clippy, formatting and rustdoc pass. Three registrations remain
ignored in the ordinary run (the internal helper and two systemd-lifetime fixtures).
GPU allocation comparisons pass. No live compositor/systemd lifetime, coverage,
mutation or end-to-end latency claim is made.
