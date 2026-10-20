# Five-target follow-up

Starting point: `adc9558` (checkpoint of the preceding GPU/history/action refactor).
Each numbered step has its own implementation commit. Raw measurements live in
`target/next-five/`; this review does not replace the full historical baseline.

## 1. Stream GPU usage into the snapshot

`gpu::Sampler::sample` now yields usage records instead of allocating another
PID-to-usage map. The main sampler consumes the entire frame into the process
snapshot; provider reads still occur before the per-process reads. Missing
observations, PID reuse and resume continue to invalidate counter baselines.
No new architectural abstraction or dependency was introduced.

The opt-in `gpu_frame_allocations` test compares streaming against collecting the
same iterator. Inputs (64 processes, two clients each) are prepared outside the
measurement. Before this change the eager implementation measured 455 allocations
and 43,244 bytes; after it, streaming measures 454 allocations and 32,860 bytes
(**24.0% fewer allocated bytes**). The collected comparison still reproduces
455 / 43,244. These are synthetic allocations, not GPU-driver latency evidence.

Verification: all-feature library suite (32 passed, one internal helper ignored),
all-target/all-feature strict Clippy, formatting and diff checks. The provider
fixture now supplies GPU counters so cache tests cover publication through the
real sampler, PID reuse, resume, exit and inactive applications. The fixture's
counter copies are test-only; production still moves counter maps/keys.

Initial RQLens architecture attempts were rejected because the live sibling
`daemon-framework` inputs changed during extraction. No mixed-input architecture
result is accepted as current evidence; final measurements must be regenerated
against stable inputs. Source-size reduction is not claimed for this step:
allocation and integration regressions add test code.
