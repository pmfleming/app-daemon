# Follow-up engineering pass

The starting point is the 25-test cleanup commit `298ddb0`. Each step is committed
locally; no remote push is part of this work.

## 2. Production boundaries

- Window ownership now lives in `service/identity.rs`, shared by actions and
  sampling. Query presentation accepts already-grouped windows and performs no
  procfs identity lookup. The service resolves identity before taking the
  resource/settings read locks.
- Sampling separates lightweight process/topology discovery, detailed resource
  reads, and publication/baseline updates. CPU accounting still observes all
  processes for energy attribution; expensive reads remain restricted to active
  application members. The existing PID-reuse and network recovery scenarios
  exercise the reorganized pipeline.
- The launch-only regression now verifies both the unowned shortcut and its
  separately grouped running window in the returned page.

Validation: 25 tests, strict all-target Clippy, formatting, and diff checks.
Performance changes are not assumed from this structural refactor; measurement
is a separate step below.

## 3. Integration failure modes

Added four private-session integration scenarios (25 unit + 4 integration tests):
D-Bus validation/subscription ownership; UWSM launch and desktop-action handoff,
reported failures, the real ten-second timeout and owner-only cancellation;
direct-launch detachment across daemon shutdown; and pending-handoff termination
with a final history save. These exercise the built daemon executable and real
D-Bus transport, not mock calls to private service methods.

Each daemon gets a private `dbus-daemon`, isolated HOME/XDG roots, and a PATH
containing only fixture launchers and a fixture `hyprctl`. No real desktop is
required or modified. Process cleanup is bounded, including detached fixture
applications. Nix check/dev dependencies now include D-Bus, bash, and coreutils.
Run `cargo test --test session --locked`; missing tools fail explicitly rather
than silently skipping these tests. One scenario intentionally takes ten seconds
to exercise the production timeout. Real UWSM/systemd and compositor behavior
remain outside this simulated-command integration boundary.

Validation: all 29 tests, strict all-target Clippy, formatting, and diff checks.

## 4. Performance evidence before further optimization

Added an opt-in `cargo bench --features benchmarks --bench workloads --locked`
harness. Normal daemon builds retain their allocator and dependencies unchanged
except for optional manifest entries. It measures query presentation, pure
identity resolution, warm sampling/allocation, and publication with blocked disk
workers. The test provider is reused only in benchmark-feature builds.

Three release runs of 500 iterations per timing/allocation phase are recorded in
`benchmarks/baseline.json`; scope and limitations are in `benchmarks/README.md`.
Median-of-medians: empty query 767 µs, search 1,098 µs, pure identity 504 µs,
sampling 369 µs, sampling with blocked disk workers 361 µs. Allocations were
stable across the three runs. These are synthetic, instrumented measurements,
not a pre/post optimization comparison or a desktop latency SLA.

## 5. Refreshed quality and coverage baseline

Re-ran RQLens producers, executed correctness/coverage/verification, and saved
`quality/baseline.json` with reproducible export instructions in
`quality/README.md`. Raw current artifacts are in `target/quality-current/`;
a frozen local copy is in `target/quality-baseline/`. The exporter rejects
incomplete or mixed-fingerprint inputs. Missing producers initially prevented
the strict completeness check; running the full required set resolved this,
without suppressing missing evidence.

Comparison with the 25-test starting point (`298ddb0`):

| Metric | Before | After |
| --- | ---: | ---: |
| Sampler orchestration SLOC | 141 | 38 |
| Sampler orchestration cognitive / cyclomatic | 2 / 6 | 0 / 2 |
| Sampler orchestration hotspot score | 47.60 | 9.18 |
| Query-page hotspot score | 31.19 | 23.13 |
| Query module locality / leverage | 97 / 55 | 100 / 58 |
| Resource module locality / leverage | 100 / 89.5 | 99.25 / 94.5 |
| Mean module locality | 99.83 | 99.70 |
| Mean module leverage | 67.10 | 67.08 |
| Production-function cognitive sum | 324 | 323 |
| Production-function cyclomatic sum | 783 | 785 |
| Production-function maximum hotspot score | 73.81 | 73.81 |
| All-code maximum hotspot score | 73.81 | 153.78 |
| All-code cognitive / cyclomatic sum | 342 / 887 | 377 / 1,041 |
| Duplication | 4.91% | 5.19% |
| Escape hatches | 0 | 0 |
| Executed tests | 25 | 29 |
| Default-feature line coverage | 60.45% | 76.59% |

The extracted detailed sampling stage is 88 SLOC, cognitive 1, cyclomatic 3;
discovery and orchestration now have explicit roles, rather than claiming the
work disappeared. The new all-code maximum is integration-fixture setup, not a
production regression. Additional test/benchmark modules change the architecture
population, so the mean scores and duplication did not improve uniformly.

Coverage now reaches the executable and D-Bus server: `daemon.rs` 82.97%,
`api.rs` 68.75%, `launch.rs` 79.82%, and `service.rs` 69.56%. Branch coverage is
unavailable in this run. These are host-specific dynamic line observations, not
proof of every failure path. No claim of measured Halstead effort is made.

All 29 tests and strict Clippy pass. RQLens completeness, test-failure, and
error-level practice checks pass, with five existing project-practice warnings.
The default aggregate threshold warnings for resources/service remain visible;
threshold enforcement was not enabled. The full baseline also records the
opt-in benchmark panic-path advisories rather than labeling every signal green.
