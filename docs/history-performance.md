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
