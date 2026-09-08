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
