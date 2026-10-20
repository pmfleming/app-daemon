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
