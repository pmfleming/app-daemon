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
