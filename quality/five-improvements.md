# Five follow-up quality improvements

Baseline: `9e981e6`. RQLens evidence before changes is retained locally in
`target/five-improvements/before/`; the existing `rqlens.toml` is unchanged.
Each section records a separately validated implementation commit.

## 1. Project launch receipts into operation results

`operation_result` borrows a receipt and copies only the three public wire fields.
Progress reporting no longer clones its process-ownership map or private unit.
Completion uses the same projection (small public-field copies rather than the
previous moves); receipt ownership/lifetime is independent of output construction.
The existing handoff/cancellation test now checks placement, retained provenance,
private-field omission, and independent event/recovered results.

Validation: `cargo test --locked` (60 passed, 3 opt-in systemd tests ignored),
`cargo clippy --locked --all-targets --all-features -- -D warnings`, formatting,
and `git diff --check` passed. No runtime allocation benchmark is claimed.
