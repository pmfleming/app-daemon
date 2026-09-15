# Vendored Shelllist libraries

These small source snapshots make Cargo and Nix builds independent of sibling
checkouts and unpublished/local Git URLs. Registry dependencies remain pinned by
the root `Cargo.lock`; Nixpkgs remains pinned by `flake.lock`.

| Directory | Upstream | Source commit | License |
| --- | --- | --- | --- |
| `daemon-framework` | https://github.com/pmfleming/daemon-framework | `47d5a6505a8fbaebb48469d8e689e2743980e416` | MIT; upstream LICENSE included |
| `shelllist-hyprland` | Shelllist's local `shelllist-hyprland` repository (no remote configured at snapshot time) | `61376dd84a0a8f5edef2da02b11b36cd0e3edbf4` | MIT, as declared by upstream Cargo.toml; no separate upstream LICENSE file |

The snapshots contain upstream manifests, library source/tests, and README files;
framework workspace metadata and LICENSE are also included. Git metadata, build
outputs, upstream lockfiles and upstream Nix flakes are deliberately excluded.
The `daemon-framework` snapshot includes the reviewed 2026-09-13 transport-routing
changes from the source commit above (committed locally upstream):

- optional typed JSONL request addresses echoed on replies;
- Rust-owned subscription event routing and consumer release;
- bounded request admission with a reserved control lane;
- cancellation failure handling and routing/churn regression tests.

The snapshot also includes the coordinated server-infrastructure extraction in
upstream commit `47d5a6505a8fbaebb48469d8e689e2743980e416`: managed subscriptions,
connection-scoped owner monitoring, task groups, resume detection, atomic/staged
files and bounded reads, blocking lanes, operation bookkeeping and event forwarding.
The source, manifests, framework README and server-infrastructure documentation
match that reviewed commit. See
[`daemon-framework/docs/server-infrastructure.md`](daemon-framework/docs/server-infrastructure.md).
The Hyprland snapshot also includes phase 4's typed work-area parser, workspace
rule resolution, socket reply-size bound and regression tests.

To update, export the same paths from an explicitly reviewed upstream commit,
replace the corresponding directory, update this table, then run:

```sh
cargo test --locked --all-features
cargo test --manifest-path vendor/daemon-framework/Cargo.toml
cargo test --manifest-path vendor/shelllist-hyprland/Cargo.toml
cargo clippy --locked --all-targets --all-features -- -D warnings
nix build --no-link
```

Review upstream license changes and the complete source diff. Do not restore
`../` dependencies or local-file flake inputs. Changes to vendored source must
be documented here and preferably contributed upstream separately.
