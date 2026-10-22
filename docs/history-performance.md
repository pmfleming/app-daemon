# History persistence performance

## Compact availability (2026-10-22)

Historical buckets retain the nine capability flags, not a 48-key heap map.
The public history query still projects all 48 keys, including false values.
Legacy maps are ignored on load, as before; legacy files without flags retain
existing conservative inference. Live resource projections are unchanged.

Release isolated probe (`cargo test --release --locked profile_history_persistence
-- --ignored --nocapture`), 24,000 deterministic buckets, three fresh processes
per side. Setup is excluded from snapshot/encoding times; no concurrent build.
The probe deliberately retains the encoded buffer and snapshot to expose their
combined resident cost; these are not measurements of the live daemon's RSS.

| Metric | Before (three runs) | Compact history (three runs) |
| --- | --- | --- |
| Snapshot ms | 101.546, 101.369, 112.927 | 6.162, 6.657, 5.771 |
| Encode ms | 74.179, 72.500, 76.034 | 44.372, 43.333, 43.031 |
| Encoded bytes | 78,510,753 | 43,638,753 |
| RSS KiB with snapshot + JSON retained | 284832, 284724, 284596 | 75932, 75936, 75844 |

Tests cover old maps, missing flags, API projection, split buckets, persistence,
retention, cursor rewrites/backfills and clipped summaries. This establishes a
large avoidable allocation cost, not proof that allocator retention explains all
of the previously reported live RSS. No live history file was rewritten by the
probe and this change has not yet been deployed.

## Streaming and dirty generations

Framework `464c394` streams through the existing staged-file durability boundary.
A release probe saving the same compact fixture measured **111.305, 112.116,
105.505 ms** including snapshot, serialization, buffered writes, file sync and
rename/parent sync. Sampled process VmHWM was **33044, 33036, 33172 KiB**; the
encoded file remained 43,638,753 bytes. Unlike the encode-only probe above this
includes storage latency, so its duration is not an encoding-speed comparison.

Successful saves acknowledge only the captured generation, inside the blocking
writer even when its awaiter is cancelled. Unchanged saves skip both snapshot
copying and writing; pruning and completed resource/energy buckets dirty the
store. Final shutdown flushes pending data before checking. Failed writes remain
retryable. Loading requests one rewrite for legacy compaction/repairs. Dirty
tracking intentionally does not promise lower write rates while new buckets
arrive every minute. Tests cover concurrent mutations, failed writes, retention,
shutdown ordering, and byte-identical public protocol schemas.

All release app-daemon ordinary tests passed (30 library, 15 session; explicit
opt-in probes and systemd fixtures excluded).
