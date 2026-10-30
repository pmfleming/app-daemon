# Resource history: embedded Turso

## Cutover

The store is `$XDG_STATE_HOME/app-daemon/resource-history-turso-v1.db`, falling
back to `~/.local/state/app-daemon/`. Turso is pinned to 0.8.3 with default
features disabled and syscall I/O. There is no remote service.

This deliberately starts fresh. `resource-history-v1.json` is never read,
imported, renamed, or deleted. Cursor version 3 rejects pre-cutover cursors.
Resource/energy response schemas and their aggregation semantics are unchanged
(apart from ordinary floating-point rounding differences in SQL aggregation).
Timestamps must fit nonnegative signed 64-bit milliseconds.

## Data and work

- `resource_points`: primary key `(target_id, timestamp_ms)`, interval start,
  compact JSON payload for a **single** point, and typed value/peak columns for
  the eight statistical metrics. Unavailable/nonfinite metrics become SQL NULL.
  Capability projection maps are produced only for response points.
- `energy_points`: the same composite key with numeric energy and source /
  confidence labels; no JSON decoding for energy overviews.
- `cursor_epochs`: persistent per-target rewrite identities.
- Separate timestamp indexes support global resource (24h) and energy (7d)
  retention. Deletion is strictly before the cutoff, retaining its boundary.

Only unfinished aggregates are kept in Rust memory, at most one resource and
one energy aggregate per active target. A sampling pass commits its completed
buckets in one transaction. Ordinary appends do not touch older buckets.
Restarted partial buckets, same-endpoint samples and backfills merge only the
relevant rows. Replacing an endpoint and rotating its cursor epoch are atomic.

Pages use a composite-index timestamp seek and `LIMIT limit+1`; only returned
payloads are decoded. Summaries use duration-clipped SQL aggregates over the
selected target/window, independent of page/cursor, without decoding payloads.
Energy overviews use `SUM`/`GROUP BY` and merge any unfinished energy aggregates.
Retained cursor anchors are checked in the DB. Retention does not invalidate
newer anchors; rewrites/backfills do.

Startup opens/validates the schema and prunes expired rows, rather than loading
every historical point. The 60-second maintenance job flushes expired idle
aggregates and performs indexed retention. Queries also flush expired buckets
and filter by current retention, even between deletes. No-op maintenance does
not write retained rows. Database pages freed by retention are reusable; file
size need not shrink. Turso's normal automatic WAL checkpointing is enabled;
clean shutdown flushes partial aggregates and requests a truncating checkpoint.

## Concurrency, durability, failures

All DB work runs on blocking workers. An owned service mutex guard travels
with each worker: cancelling an async caller cannot release serialization while
its transaction still runs. Query/sampling/maintenance/shutdown share this gate;
there is no stale snapshot writer or separate in-memory historical cache.

The database uses `synchronous=FULL`. Completed resource/energy rows and cursor
metadata commit together. Only after success are the proposed unfinished
aggregates installed. A known rollback leaves the previous pending aggregates
intact, so flushing can be retried without double-counting. A failed sampling
pass is reported and not accepted; unavailable storage can therefore produce
measurement gaps. Pending aggregates are volatile: an unclean exit can lose
unfinished resource/energy buckets; clean shutdown commits both.

Commit errors with uncertain outcomes or failed rollbacks fence the store until
restart, avoiding additive-energy replay. Read/write failures are not converted
into empty histories. History API storage failures return `daemon-unavailable`;
invalid bounds/cursors return `validation-error`. Background failures are logged.
Unknown schema/application IDs, database-open failures and preexisting empty
files fail startup; no automatic reset or destructive repair is attempted.
Corruption discovered during reads is reported, not silently dropped. Startup
does not scan/decode every payload as an integrity check.

The state directory is user-owned/private (0700); DB, lock and WAL sidecars are
0600 with symlink/hard-link checks. A lifetime advisory lock prevents two daemon
instances from owning the same history. Do not remove the lock file while a
process is using the store.

## Recovery / backup

Stop the daemon before copying, replacing, or moving the store. Preserve the
DB **and any WAL/SHM sidecars** together; never discard a WAL to fix a failure.
Keep the original files for diagnosis. If historical data is expendable, move
the stopped store and sidecars aside and start with a fresh path. Do not copy
only the main DB while the daemon is running. Legacy JSON remains available
separately but is not a supported restore source for this schema.

## Qualification

`cargo test --locked --all-features` covers persistence/reopen, retention
boundaries, cursor lifecycle, split/partial bucket merging, SQL/reference
summaries, index query plans, rollback/retry, ambiguous commits/fencing,
private permissions, exclusive ownership, rejected corrupt/incompatible stores,
WAL recovery after a process exit without destructors, cancellation-safe worker
serialization, and actual daemon shutdown through the private-session integration
suite. Validation: 52 library tests and 18 ordinary session tests pass;
`cargo clippy --locked --all-targets --all-features -- -D warnings` is clean.
Explicit opt-in tests remain excluded from the ordinary suite.

`cargo test --release --locked profile_turso_history -- --ignored --nocapture`
uses only a temporary directory: seed 35,000 points, append ten points in one
transaction, query a 100-point page, and report latency plus changed-row count.
One local release run: seeding **6,946.657 ms**, ten-point append **6.279 ms**
with **10 changed rows**, and the page plus full-window SQL summary **10.379 ms**.
This is an isolated resource-only fixture, not a claim about measured live
disk-write savings. No live history was opened or changed by the probe.
