# Test pruning: 70 → 47

Baseline: `0b8ff16`. This is a one-time reduction, not a quota for future tests.
Production behavior, public APIs, dependencies and fixtures are unchanged.

## Count and scope

Counted with `cargo test --all-features --locked -- --list`, including existing
ignored registrations but not counting subprocess helpers twice:

| Suite | Before | After |
|---|---:|---:|
| Library | 50 | 31 |
| Private-session integration | 17 | 14 |
| Opt-in systemd, including its child helper | 3 | 2 |
| **Total** | **70** | **47** |
| Normally runnable | 65 | 44 |

Removed **23 registrations (32.86%)**, retaining **67.14%**: the nearest whole-test
result to 67%. Rust source under `src/` and `tests/` fell **14,992 → 14,329 lines**
(−663). Removed code was deleted, not hidden behind ignores, flags or test runners.
Three pre-existing ignored registrations remain: the stderr subprocess helper,
the real-systemd scenario and its child helper. The systemd scenario still runs
both service and desktop-handoff cases; these were identical wrapper tests.

## Selection and retained protection

| Removed/replaced registrations | Net reduction | Retained coverage |
|---|---:|---|
| Separate protocol registry/resource and private event-envelope tests | 2 | Live declared-method dispatch/frozen registry, resource fixture field shapes and availability, public gap fixture; live signals assert envelope and routing |
| Owned-vs-borrowed cursor wire compatibility test | 1 | Cursor behavior, target validation, restart, expiry and rewrite invalidation; no private encoding-layout constraint |
| Separate restart and pruning fixtures | 1 | One cursor lifecycle includes inclusive retention cutoff, surviving anchors, narrowed windows, restart, gaps and clock rollback |
| Separate bucket-splitting fixture | 1 | Persistence fixture now straddles buckets, conserves odd/maximum counters and energy, checks partial endpoints, clipped intervals, availability and seven-day retention |
| Synthetic GPU counters and standalone DRM parser test | 1 | Raw DRM input feeds engine aggregation, busy caps, PID reuse and resume checks |
| Standalone TCP-info parser test | 1 | Socket lifecycle now consumes decoded kernel messages; attribution, closure, unavailable counters and baseline recovery remain |
| Separate stat and memory parser registrations | 1 | Procfs acquisition contract retains identity, malformed/truncated input, saturation, units, missing data and swap precedence; drops exact allocation counts/private iterator checks |
| Separate allocated-inode arithmetic test | 1 | Filesystem walk covers hard links, symlinks, first observations, separate devices, saturation, budgets and deadlines |
| Separate sampler resume fixture | 1 | Cache discontinuity test covers CPU/I/O/network/energy baselines; GPU and cgroup fixtures check observable resume behavior instead of private map emptiness |
| Separate launcher-sibling/D-Bus discovery fixture | 1 | Windowless sampling/query/history fixture includes unrelated siblings and stable D-Bus units |
| Separate retained-grandchild boundary fixture | 1 | Boundary lifecycle covers migration, foreign reparenting, nested owners, shortcuts and conflicts without editing private anchors |
| Launch provenance internal-state test | 1 | Ownership/resource PID-reuse and reparenting regressions, real launch handoffs, migrated-scope placement and restart recovery remain |
| Launch-only query fixture and cgroup alias permutation test | 2 | Shared listing/search fixture checks shortcut isolation and focused-window ranking; catalog fixture checks escaped nested scopes; resource/live ownership fixtures retain application boundaries |
| Settings serialization unit test | 1 | Category-to-workspace launch integration now restarts the daemon and verifies persisted settings |
| Private post-handoff outcome test | 1 | Live placement integration checks rejected/unconfirmed movement and successful placement followed by focus failure, retaining the launch receipt |
| Separate Hyprland selector/event classifier test | 1 | Injection rejection stays beside configured-instance-only dispatch; live placement covers named/special workspaces |
| Watcher exact retry/backoff policy test | 1 | Partial watch installation, profile replacement, compositor failure/reconnection and refresh behavior remain; exact retry scheduling is no longer prescribed |
| Separate oversized direct-bus test | 1 | Request-validation fixture checks valid and malformed oversized effects before parsing/launch; obsolete-interface-only rejection check removed |
| Separate explicit/launch placement fixtures | 1 | Shared compositor confirmation fixture distinguishes failed moves from successful launch with partial failure; redundant rejection permutation removed |
| Ignored manual bridge reliability measurement | 1 | Actual two-bridge ownership, oversized requests and recovery test retained; manual measurement was not a regression gate |
| Duplicate opt-in systemd wrapper | 1 | Same lifetime helper runs both modes from one registration |
| **Total** | **23** | |

Focused tests for shutdown/save ordering, operation admission/cancellation,
descendant stderr lifetime, disk worker bounds, metadata revision invalidation,
launch timeouts and ownership recovery rejection remain independent.

## Verification and tradeoffs

Passed:
- All-feature tests: **30 library + 14 integration**, with three existing ignores.
- Strict all-target/all-feature Clippy, package formatting and `git diff --check`.
- A fresh all-feature LLVM coverage run.

App production-source line coverage in the paired runs was **5,972/6,443 (92.69%)
→ 6,003/6,443 (93.17%)**. The comparison excludes dedicated/inline tests,
benchmark files and the test provider, and maps unchanged lines across edits.
It does not include sibling dependency coverage. **21 formerly covered lines
became uncovered**, chiefly event classification, retry-policy branches and an
error-label fallback; **52 became covered**, mostly public offline event fixtures
plus focus-failure fallback. The aggregate gain is not proof of stronger defect
detection, branch equivalence or a runtime speedup. No mutation audit was run.

The real user-systemd cases were compiled but not run; they retain their existing
opt-in boundary. No cross-project RQLens policy pass is claimed.

Local evidence: `target/test-pruning-70-to-47/` contains before/after listings,
source snapshot, test/Clippy logs, LLVM JSON/LCOV exports, the comparison script
and `production-coverage-delta.json` with exact lost/gained source lines.
