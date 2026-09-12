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
