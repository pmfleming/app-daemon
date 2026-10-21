# Five follow-up improvements

Starting point: `5bc7584` (separate checkpoint of the preceding refactor).
Measurements use the unchanged `quality/regression.toml`, local RQLens binary,
and `target/next-five-{0..5}.json` snapshots. The historical policy baseline is
not replaced. Macro-expansion limitations from the preceding review still apply.

## 1. Defer query hydration

`service/query.rs` now ranks lightweight candidates, then aggregates application
and window resources only for selected rows. Metadata ranking still sees the
original client titles/classes; running state still includes background processes.
The candidate does not allocate a placeholder resource-availability map. Identity
metadata remains eager; this does not claim zero allocation for discarded rows.

A regression test covers window-title search, catalog/unmatched rows, category
filtering, pagination, focus and complete selected-row payloads. All-feature tests
and strict all-target/all-feature Clippy pass.

Release synthetic benchmark, 30 iterations, same fixture and calling-thread
allocator instrumentation (timings are advisory):

| Query | Allocations before → after | Bytes before → after | Median µs before → after |
| --- | ---: | ---: | ---: |
| Empty (500 of 1,000 rows) | 165,003 → 102,004 | 21,335,634 → 12,940,634 | 10,255 → 5,297 |
| Search | 169,037 → 20,498 | 21,613,732 → 1,335,958 | 9,816 → 1,278 |

RQLens tradeoffs: production cyclomatic sum 1,199 → 1,200 and effort
4,365,912 → 4,375,248; cognitive sum stays 540. Production-file lines +23;
regression tests +62. Mean leverage improves 24.7761 → 25.3731, locality declines
98.5560 → 98.4552 (additional test dependencies and catalog fan-in). These static
costs buy measured allocation reductions, not a universal metric improvement.

## 2. Stream history selection and summaries

`history::window_points` centralizes endpoint/overlap selection and returns a
borrowed double-ended iterator. `summary::summarize` consumes that iterator once,
tracking the same count/last endpoint for its revision. Queries no longer build a
`Vec<&ResourceHistoryPoint>` proportional to the retained window; only returned
page payloads are cloned. This is a source-level allocation guarantee, not a
measured latency claim.

Extended the cursor/restart regression with a summary-only right-boundary overlap:
it contributes observed duration but neither advances pagination nor changes the
summary on an exhausted page. Full all-feature tests passed before extracting the
selection helper; history tests and strict Clippy passed again afterward.

Step-local production effort falls 4,375,248 → 4,368,664; cognitive stays 540,
cyclomatic adds one helper (1,200 → 1,201). Production-file lines +4 and test lines
+18. All-code effort rises due to added assertions in the existing long cursor
scenario. Locality, leverage, clone-site counts and escape hatches are unchanged.

## 3. Allocation-free fixed-field procfs parsing

CPU/cgroup I/O, process I/O, smaps and status parsers select borrowed fields into
fixed-size option arrays rather than temporary hash maps. Selection preserves
last-*valid*-duplicate behavior, supported zero versus missing observations,
SwapPss precedence, strict required cgroup fields and saturating arithmetic.
Tests cover these cases and enforce zero parser allocations with the benchmark
feature enabled.

The same new parser workload was measured with the original map implementation
before switching algorithms, then afterward (release, 100 iterations): **7 → 0
allocations**, **1,112 → 0 bytes**, median **1.363 → 0.461 µs**. Raw reports are
`/tmp/app-parser-before.log` and `/tmp/app-parser-after.log`; reproduce the current
workload with `APP_DAEMON_BENCH_ITERS=100 cargo bench --locked --features benchmarks
--bench workloads` (`parsing/procfs`).

All-feature tests and strict Clippy pass. Production effort decreases
4,368,664 → 4,367,059; duplicated lines 656 → 631. Cognitive +3 and cyclomatic +5
include the explicit selector loop and newly exposed parsing/benchmark helpers.
Authored source grows 113 lines including fixed schema arrays, benchmark fixtures
and regression assertions; this is an allocation/duplication win, not a size win.
Architecture scores and clone/escape counts remain unchanged.

## 4. Encapsulate network continuity

`resources::network::Sampler` now owns descriptor observations, counters and
availability. Resource orchestration no longer depends on three parallel fields
or on CPU-counter state to validate socket owners. Owners are keyed by PID/start
time; counter history is discarded when no continuing observed owner remains.
This also fixes a concrete attribution gap: reuse of **both PID and socket inode**
previously subtracted the old process's counters instead of establishing a new
baseline. A live co-owner still preserves continuity for a shared socket.

Extended cache regressions cover initial bytes of newly opened sockets, dump
failure/recovery, reused PID+inode, resume and exit. All-feature tests and strict
Clippy pass. The source analyzer required a fully qualified inherent-impl owner;
no partial-evidence exception was added.

Production effort falls 4,367,059 → 4,347,903; cognitive stays 543, cyclomatic +3
for the sampler/accessor helpers. Source +21 lines (+8 production files, +13 tests).
Mean leverage 25.3731 → 25.5224; locality 98.4552 → 98.4104 because the resource
orchestrator now has an explicit network-module dependency. Duplicate lines +7
from additional cache assertions; clone/escape counts are unchanged.

## 5. Fail closed on incomplete netlink dumps

The transport uses `MSG_TRUNC` to detect oversized datagrams and a shared 500ms
receive deadline per address-family dump (rather than a fresh timeout for every
packet). Empty datagrams and expired deadlines terminate the read. The budget
covers receiving the dump, not kernel execution of the initial send.

Checked, shared frame decoding validates message/attribute lengths and alignment.
Numeric reads return `Option` instead of potentially panicking on a short slice
or fabricating zero via a fallback. Interrupted dumps, error/overrun messages,
failed DONE status and malformed requested observations make the provider return
unavailable; partial counters are discarded by the caller. Valid short legacy
`tcp_info` remains unsupported accounting, not a fabricated zero.

Tests cover every truncated prefix of a valid observation, malformed attribute
lengths, interrupt/error/overrun/DONE handling, sequence isolation, legacy layouts,
and actual local datagram truncation/empty packets/deadlines. No live netlink or
compositor fixture is required. Message disposition is separate from traversal,
keeping the production cognitive maximum at 8.

Step-local production effort 4,347,903 → 4,365,691, cognitive +4, cyclomatic +25:
fallible decoding adds real checked failure paths. Source +151 lines consists of
34 production lines and 117 inline test lines. Clone sites, architecture scores,
escape-hatch and production reliability-finding counts are unchanged.

## Final verification and tradeoffs

All five steps were committed independently after validation, in addition to the
initial checkpoint. Final all-feature tests pass **28 library + 15 session tests**;
the subprocess helper also runs via its parent. Three registered helper/live
systemd cases remain ignored by the outer harness. Default-feature tests, strict
all-target/all-feature Clippy, formatting, rustdoc, Python gate tests and diff
whitespace checks pass. Live systemd lifetime tests and coverage were not run.

| RQLens/source metric, entire five-step series | Before | After |
| --- | ---: | ---: |
| Production cognitive sum / max | 540 / 8 | 547 / 8 |
| Production cyclomatic sum | 1,199 | 1,234 |
| Production effort sum | 4,365,912 | 4,365,691 |
| All-code effort sum | 8,317,248 | 8,611,394 |
| Mean leverage | 24.7761 | 25.5224 |
| Mean locality | 98.5560 | 98.4104 |
| Duplicate lines | 656 | 638 |
| All-code clone sites | 141 | 141 |
| Rust lines, including tests/benchmarks | 13,910 | 14,302 |
| Escape hatches / production reliability findings | 0 / 8 | 0 / 8 |

These five prioritize measured allocation reductions and correctness; they are
**not** an across-the-board static-metric or source-size reduction. Added safety
checks, explicit schemas, benchmark fixtures and regression tests explain the
size/complexity tradeoffs. Net production effort is effectively flat. Leverage
gains partly reflect new test consumers, not just production reuse.

The strict regression gate still fails, including the new tradeoffs above and
pre-existing historical instrument/inventory differences. Neither the tracked
baseline nor its policy was relaxed. Reproduce fresh evidence/comparison with:

```sh
python3 scripts/quality_regression.py --baseline target/next-five-0.json
```

The local snapshots preserve fingerprints and module-level results. They are
review evidence, not accepted replacements for `quality/regression-baseline.json`.
