# Robustness implementation

Each step is committed locally; no remote push is part of this work.

## 1. Safe launch/window placement

- Serialize operations for the same application using weakly retained, cancellation-safe locks.
- Read a fresh compositor snapshot after acquiring the lock.
- Give UWSM launches unique explicit service units; require a new address and exact launch-unit membership before moving/focusing a window.
- Refuse ambiguous or unprovable placement (including launches handed off to an existing singleton process), and describe that limitation in the operation result instead of moving an unrelated window.
- Unit tests cover exact unit matching, existing addresses, ambiguity, unique units and per-target serialization.

Validation: 35 tests, strict all-target/all-feature Clippy, formatting.

## 2. Catalog launch-metadata invalidation

Hash complete ordered desktop-entry groups and the source path, not only presentation fields. Tests independently change Exec, Terminal, Path, DBusActivatable, and action Exec while preserving presentation, and verify stable hashes for unchanged entries.

Validation: catalog regression tests and formatting.

## 3. History restart/clock recovery

Maintain sorted, unique resource and energy buckets, merge restarted partial buckets with duration-weighted metrics and preserved peaks/counts, and normalize older malformed ordering on load. Cursor v2 persists per-target epochs: normal appends/restarts preserve polling; rewrites/backfills explicitly invalidate old cursors instead of skipping data. Empty polls retain the cursor. Pruning is also applied on history queries.

Validation: six history tests including restart persistence, duplicate repair, clock rollback, cursor resync, and pruning; strict Clippy and formatting.

## 4. Independent cgroup capabilities

CPU, memory and I/O counters now use independent optional values. Missing/malformed controllers preserve available procfs metrics, readable idle I/O remains supported zero, and controller recovery establishes an independent baseline. Memory gauges remain available on the initial sample.

Validation: 16 resource tests, including fixture-filesystem controller reads and sampler fallback/recovery assertions; strict Clippy and formatting.

## 5. Checked direct-launch handoff

Observe gtk-launch exit status under the same bounded handoff policy as UWSM. Capture at most 8 KiB of diagnostics while draining the pipe, and do not wait for descendant stderr EOF. Timeouts/cancellation kill the pending helper, not a successfully handed-off application.

Validation: helper subprocess tests for success, rejection/exit 42, missing executable, timeout, bounded diagnostics and inherited stderr; strict Clippy and formatting.

## 6. Launch lifetime isolation

Add a systemd-run fallback: GTK handoffs use independent scopes; long-running terminal/action executables use exec-type, cgroup-lifetime services with inherited environment and working directory. UWSM actions also use explicit unique units. Refuse unmanaged direct launching in service cgroups when neither helper exists. Keep the daemon service's normal control-group cleanup policy.

Validation: 46 tests, including backend selection and scope/service command contracts, strict Clippy and formatting. Real user-systemd lifetime behavior remains an explicit integration boundary; fixture integration is restored in a later step.

## 7. Recoverable operation outcomes

Retain bounded, expiring terminal outcomes and current running state with owner-scoped status lookup. Add applications.operation.status to the protocol contract; status responses do not create new client correlation lifetimes. Lagging streams emit resync-required. Bound active operations globally/per owner, and ensure cancellation cannot be overwritten by late running/completed updates.

Validation: 48 tests including retention, expiry, admission, ownership, cancellation races, status lookup and stream-control correlation; strict Clippy and formatting.

## 8. Coordinated shutdown and persistence

Shutdown closes request admission, drains admitted calls, cancels operations, stops owned monitor/event tasks, and joins the sampler before saving final history. Persistence is serialized from snapshot creation through completed atomic write; the serialization guard lives in the blocking writer so cancellation cannot release it early. Repeated shutdown is idempotent.

Validation: 50 tests, including delayed sampling/older-write shutdown ordering, admission rejection and operation cancellation; strict Clippy and formatting.

## 9. D-Bus-only catalog entries

Accept DBusActivatable entries without Exec. Launch and desktop actions use validated org.freedesktop.Application addresses, forward activation platform data, and bound activation calls. Exec fallback is used only when available; unsupported activation fails explicitly. D-Bus singleton placement remains conservative.

Validation: 52 tests including D-Bus-only catalog/action visibility and address validation; strict Clippy and formatting. Actual private-bus activation is covered by the integration step.

## 10. Window-independent application accounting

Move named-unit identity resolution into the catalog and independently discover the current user's application cgroups during sampling. Retain observed process/descendant identities across window closure and reparenting, prune on exit/PID reuse, exclude separately owned application children, and never attribute launch-only/generic daemon scopes. Publish background running state, topology revisions, history and energy without requiring windows. Per-window views remain distinct from application totals.

Validation: regression tests exercise windowless cgroups through sampling/query/history, orphaned helpers, PID reuse, separate child ownership and named-unit formats; strict Clippy and formatting. Unidentified, never-observed unmanaged processes remain explicitly outside attributable application accounting.

## 11. Standalone source builds

Vendor the three small Shelllist libraries from recorded commits, including available upstream licenses and update/provenance documentation. Cargo now resolves only in-tree path dependencies; remove all local-file flake inputs and sibling-copy hooks. Add explicit Nix check tools and document cold-build/runtime requirements.

Validation: 56 tests and strict Clippy pass using the vendored libraries. An isolated source export under /tmp builds and passes all 56 tests with --locked --offline, without sibling checkouts. Nix flake evaluation succeeds. An offline Nix package build cannot complete because the pinned Nixpkgs bootstrap sources/substitutes are absent from the host store (stage0-posix-1.9.1-source); this is recorded rather than claiming a successful package build. A subsequent network-enabled build in step 12 succeeded, including sandbox integration checks.

## 12. Restored integration boundaries

Restore eight executable/private-bus integration scenarios with an in-tree D-Bus configuration, isolated environment, bounded fixture process cleanup, and mock command/event sockets. Cover validation/ownership, UWSM receipts and ten-second timeout, owned cancellation, late operation lookup, systemd fallback rejection/lifetime, metadata-only refresh/terminal working directory, D-Bus-only activation/actions, shutdown and compositor recovery. Add two opt-in real-systemd lifetime scenarios exercising production launcher code from a temporary host service, for both application services and GTK scopes.

Validation: 56 unit + 8 private-session integration tests pass, as do strict all-target/all-feature Clippy and formatting. Both opt-in real-systemd lifetime tests pass on this host. The network-enabled `nix build --no-link --max-jobs 2` succeeds; its sandbox check phase passes all 64 default tests (real-systemd tests are intentionally ignored there). No real graphical applications are launched by these tests.
