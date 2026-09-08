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
