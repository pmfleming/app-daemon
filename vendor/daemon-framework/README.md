# daemon-framework

Shared Rust infrastructure for the Shelllist daemon family.

## Workspace crates

- `shelllist-daemon-core` — runtime-independent protocol, envelope, JSONL wire, fixture, and secure state helpers.
- `shelllist-daemon-tokio` — Tokio and session D-Bus transport, ordered output, ownership, subscription, and shutdown helpers.
- `shelllist-protocol-js` — build tool that generates frontend constants from daemon-owned protocol registries.

The Shelllist-owned fuzzy ranking process lives with the frontend. Domain policy and frontend ranking do not belong in this infrastructure workspace.

Domain policy remains in `app-daemon`, `bar-daemon`, `bt-daemon`, `clip-daemon`, and `nm-daemon`. This workspace contains only reusable process infrastructure and services.

## Development

```bash
nix develop
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
nix flake check
rqlens measure hotspots
```

Generate a JavaScript protocol binding by piping a daemon registry to:

```bash
nix run .#protocolBindings
```
