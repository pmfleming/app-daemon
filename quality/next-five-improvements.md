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

## 2. Keep open and mapped files separate until aggregation

`ProcessFiles` now caches the two raw observations instead of cloning the complete
open-file map into a precomputed union. The response aggregates open files alone
for open-file size and both sets for referenced size. Max-by-inode and temporary
classification remain associative, so overlapping observations preserve the old
results without the duplicated cache. Fixtures no longer clone their open maps.
The resource regression covers open/mapped overlap, mapped-only temporary files,
and duplicate mappings across processes.

Validation: all-feature tests and strict all-target/all-feature Clippy pass;
RQLens hotspots regenerated. No resource wire fields changed.

## 3. Borrow cursor fields when encoding

The internal cursor schema is generic over its text representation: encoding uses
`&str`, while decoding still owns `String` fields. This removes two temporary
string allocations per encoded cursor without restricting escaped JSON strings.
A regression compares the exact base64 output with the former owned version-2
representation, including Unicode, quotes and backslashes, and checks target and
epoch validation.

Validation: all-feature tests and strict all-target/all-feature Clippy pass;
RQLens hotspots regenerated. The cursor version and serialized field order remain
unchanged.

## 4. Isolate memory parsing and normalize units once

The procfs reader now delegates to pure rollup/status parsers. KiB-to-byte
conversion is centralized in the numeric acquisition helper; private-memory
components still use saturating addition, and readable zero remains distinct
from unavailable data. Rollup read failures still fall back to status; a readable
partial rollup is not silently replaced.

Table-driven tests cover both formats, missing/malformed values, zero PSS/RSS,
SwapPss precedence (including zero), Swap fallback, overflow, and a missing PID.
The combined Halstead effort of the reader and its parsing helpers falls from
36,461 to 19,308; extraction is included in that comparison.

Validation: all-feature tests and strict all-target/all-feature Clippy pass;
RQLens hotspots regenerated.

## 5. Bounded disk scanning with saturating totals

Directory scanning is separated from multi-root orchestration. `AllocatedFiles`
retains only device/inode identities, instead of a byte value for every visited
file, and updates one saturating total. This removes the unchecked final sum
(which could panic in debug builds or wrap in release builds). Deduplication still
keeps the first observation, and budget/deadline exhaustion still returns no size.

Tests cover hard links, repeated roots, root and nested symlinks, missing/non-directory
roots, budget exhaustion, deadline expiry, cross-device inode identities and total
overflow. The largest disk-walk cognitive complexity falls from 8 to 4.

Validation: all-feature tests and strict all-target/all-feature Clippy pass;
RQLens hotspots regenerated.

## Aggregate results and tradeoffs

Production-function metrics use the preceding review's path/name exclusions for
tests and benchmarks. All extracted helper functions are included, not just the
shorter callers.

| Production metric | Baseline | After five improvements |
| --- | ---: | ---: |
| Cognitive sum | 563 | 560 |
| Cyclomatic sum | 1,205 | 1,211 |
| Halstead effort sum | 4,434,720 | 4,419,760 |
| Functions | 467 | 471 |

These are primarily allocation, ownership and correctness improvements, **not a
uniform reduction of every metric**. Extracted/testable helpers increase aggregate
cyclomatic complexity. Total Rust source grows by about 177 lines, predominantly
regression tests; no tests or public APIs were removed to lower the counts.
All five steps passed the full all-feature test suite and strict Clippy before
being committed. The final suite has 49 passing library and 16 passing integration
tests, with five registered tests/helpers still ignored. No runtime speedup is
claimed; the process-parser allocation assertion is the focused executed evidence.

Static metrics remain advisory/partial because framework-generated method items
are not inventoried; passing tests do not include the ignored live-systemd fixtures.
The sibling framework changed independently during the session. RQLens rejects
measurements whose inputs change during extraction; rejected runs were not used.
Final raw measurements should be interpreted with their recorded fingerprints,
not as a complete policy/coverage baseline.
