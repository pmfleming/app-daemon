# Robustness implementation

Each step is committed locally; no remote push is part of this work.

## 1. Safe launch/window placement

- Serialize operations for the same application using weakly retained, cancellation-safe locks.
- Read a fresh compositor snapshot after acquiring the lock.
- Give UWSM launches unique explicit service units; require a new address and exact launch-unit membership before moving/focusing a window.
- Refuse ambiguous or unprovable placement (including launches handed off to an existing singleton process), and describe that limitation in the operation result instead of moving an unrelated window.
- Unit tests cover exact unit matching, existing addresses, ambiguity, unique units and per-target serialization.

Validation: 35 tests, strict all-target/all-feature Clippy, formatting.
