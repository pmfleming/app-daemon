# daemon-framework

Shared Rust infrastructure for the Shelllist daemon family.

## Workspace crates

- `shelllist-daemon-core` — protocol/envelopes, JSONL wire, fixtures, atomic/staged files, bounded reads, and owner-scoped operation bookkeeping.
- `shelllist-daemon-tokio` — D-Bus/JSONL transport, managed subscriptions, connection-scoped owner monitoring, task groups, bounded blocking lanes, resume detection, event forwarding, and async file helpers.
- `shelllist-protocol-js` — build tool that generates frontend constants from daemon-owned protocol registries.

The Shelllist-owned fuzzy ranking process lives with the frontend. Domain policy and frontend ranking do not belong in this infrastructure workspace.

Domain policy remains in `app-daemon`, `bar-daemon`, `bt-daemon`, `clip-daemon`, and `nm-daemon`. This workspace contains only reusable process infrastructure and services.

## Server infrastructure

See [server infrastructure](docs/server-infrastructure.md) for lifecycle guarantees,
policy boundaries, migration guidance, and cross-repository deployment steps.

## Routed JSONL clients

The existing `call`, `subscribe`, and `cancel` messages accept optional bridge-local
`route` metadata. The bridge echoes it on success, domain-error, overload, and
transport-error **responses** without forwarding it to the domain D-Bus API:

```json
{"op":"call","id":"view::page","method":"clipboard.history.query","params":{"limit":200},"route":{"consumerId":"view","localId":"page","generation":1,"kind":"call"}}
```

`kind` is `call`, `subscription`, `base-subscription`, or `control` and must match
the request operation. `generation` is the frontend transport generation; the
frontend rejects responses/events from retired generations. Unrouted clients keep
their existing response format. Unsolicited transport-error notifications remain
global and invalidate the connection rather than replaying its requests.

Ordinary request addresses live with bounded request tasks, not a long-lived
routing dictionary. The output actor retains addresses only for live
subscriptions and attaches their route to subscription events, including events
buffered before the subscribe reply. Cancellation and connection reset remove
ownership. Domain operation correlation remains independent of subscription
routing.

`{"op":"release","id":"view::release","route":{"consumerId":"view","localId":"release","generation":1,"kind":"control"}}`
releases acknowledged subscriptions for that consumer/generation. A frontend
must still cancel IDs in **late subscribe replies** to a destroyed consumer;
those replies retain their address. Closing stdin drains accepted calls and
cancels the remaining tracked IDs. Failed cancellations retain ownership for
later cleanup.

Admission is bounded before spawning tasks. Overload produces an addressed error
and is never automatically replayed. Cancellation/release use a separate bounded
control lane so ordinary call saturation cannot block cleanup.

Deploy routed frontends with rebuilt daemon client binaries. There is deliberately
no frontend fallback to a per-request JavaScript object table. All Shelllist
clients use this shared implementation; remember to refresh app-daemon's vendored
copy as well as the Nix framework input.

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
