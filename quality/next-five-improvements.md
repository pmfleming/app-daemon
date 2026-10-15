# Five follow-up quality improvements

Baseline: `459173c` (the previous review, checkpointed separately).
Measurements use the local `rust-quality-lens` binary and unchanged `rqlens.toml`.
Raw per-step evidence is retained in `target/next-five/` (ignored).

## 1. Allocation-free process stat parsing

`process_stat_fields` now returns a borrowed whitespace iterator instead of
allocating a vector for every process. Identity and resource readers consume it
in kernel field order, documented beside the skip counts. Names containing spaces
or parentheses, truncated records, invalid counters and saturating CPU totals are
covered. The all-feature test asserts **zero allocations** for both parsers.

Validation: all-feature tests and strict all-target/all-feature Clippy pass;
RQLens hotspots regenerated. No public API changes or test removals.

Static metrics remain advisory/partial because framework-generated method items
are not inventoried; passing tests do not include the ignored live-systemd fixtures.
