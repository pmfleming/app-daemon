# W1-APP native frontend fixture quality review

2026-10-26. Bounded quality continuation of implementation `b76bc15`, original
baseline `0e6de17`. Not integration, rollout or a compiler-complete static pass.
Production Rust, service policies, wire schemas, limits and bridge defaults are
unchanged; this producer refactor is test-only.

## Reviewed scope and tools

Full original-baseline diff: `tests/session/socket.rs` and
`tests/session/socket/payload.rs`; no inherited seeds or generated changes.
Measured `src` + `tests`, including nested support and payload fixtures, with
pinned Rust Quality Lens `d28f3d936fa3c0d4c0a7ea753f436517077cb37d`:
`hotspots`, `clones`, `reliability`, `test-quality`, `api-health`.

The owned static config uses the pinned source's **rust_helpers/Cargo.toml**.
Normal analyzer-managed **cargo build --locked** of rust_facts/ast_hasher into
this task's private XDG cache was performed, as authorized. No main CLI rebuild,
installation, tool-source or lock modification. Helper digest `6a098384d350404d`;
main CLI SHA-256 `622b2c793a574781cd40a190a457f58af4b8c222ee5efce04c4e77488c50929a`.
No measure-all, verify, audit/coverage campaign or compiler macro expansion.

## Findings and justified refactor

BEFORE: 772 hotspot records, 63 clones / 739 duplicated lines, 169 reliability
findings, 20 test-quality advisories, 21 API-health rows. No reliability/API
finding points into either modified Rust file. Clone groups touching socket.rs
are unchanged explicit foreign-owner/crash vectors; their distinct phases remain
readable. Result/ensure! assertions and explicit private native/payload prerequisites
explain the no-direct-assertion/ignored advisories; no tests/skips were removed.

Manual lifetime review of the measured native helper found a retained negative-
path gap: timeout or pipe-I/O failure relied on kill-on-drop/background reaping.
The helper now explicitly kills **and awaits/reaps** that private child, bounded
by a two-second cleanup deadline, before returning the original I/O/deadline error.
Normal 20-second probe deadline, 32 KiB/stream diagnostics and all identity/peer/
no-launch checks remain intact. This is not a static rule's claimed detection.

One focused regression reuses the existing safe Session constructor and an
explicit private shell entrypoint that records its PID and execs only `sleep`.
It exercises the actual helper timeout and asserts the PID is absent from /proc
before the helper returns. It needs no real Quickshell or host endpoints, does
not execute a user application and does not claim a mutation-sensitivity score.

AFTER: 773 hotspot records; clones 63 / 739 lines, reliability 169, test advisories
20 and API-health rows 21 unchanged. Test inventory 73 → 74 from the new lifetime
case. The helper grows **73 → 81 SLOC, cyclomatic 7 → 10, cognitive 2 → 3**; this is
intentional explicit cleanup ownership, not a complexity-score improvement or
mechanical splitting. No thresholds, exclusions or suppressions changed.

## Evidence and limits

Every Rust envelope remains **partial**: the unchanged `declare_methods!` item
macro in `src/protocol/methods.rs` is not expanded/inventoried. No unreadable files,
parser recovery or partial source-metric files were observed. Authored syntax and
Cargo identity do not establish generated-item/compiler ownership completeness.

Private reruns passed:
- Formatting and all-target/all-feature strict Clippy.
- Ordinary debug: 46 library + **18 session** cases, including the new timeout/reap
  case; existing live-systemd and explicit-only skips remain.
- Explicit exact-result/+1 and all four payload prerequisite cases.
- Three actual native Quickshell ordinary+near-limit passes, each with a complete
  **16,707,950-byte UTF-8 comment**, healthy logical/native peers and no launch.
- Sibling Nix buildInputs/appDaemonContract/qmlTests; release app 46 library + 18
  session passes. Full Qt development/Nix: 469 passed / one existing renderer skip.

Fresh family debug daemon SHA-256 (unchanged production binary):
`5bfb28d1942c2e074c5db079eb45793bc8b959115d59a5f5824216d47ed1657c`.
Updated session-test ELF SHA-256:
`2e063ad50bc8fed26e1e0e36f3ed7a2c0b0d41dd8e613e952c3c2888b5d14b88`.
Rust/Cargo 1.95.0; Qt 6.11.1; Quickshell 0.3.0.

Exact commands/configs, raw paired artifacts, triage, identities and final clean-
head extraction are under task
`/home/laufan/Projects/shelllist-orchestration/20261026T202406Z/tasks/06-application-frontend/quality/`:
`measure-static.sh`, `check-affected.sh`, `rust-static.toml`, `before/`, `after/`,
`final/`, `TRIAGE.md`, `comparison.json`, and `affected-and-after.log`.
All heavy commands use this run's check.sh shared lock/private environment.
Owned runner waits/reaps its direct child and completion is followed by an empty
process-group check; no event/exit marker substitutes for child ownership.

This remains large-record routing, not whole-panel maximum rendering, sustained
load, process-crash/effect-receipt-loss, peak-memory or final performance proof.
Fake catalog/compositor/handoffs, private no-activation bus/HOME/XDG and unavailable
frontend host buses remain required. The paired frontend quality record is
`shelllist/tests/application-frontend-quality.md`. Coordinator review is required.
