# Vendored Shelllist libraries

These small source snapshots make Cargo and Nix builds independent of sibling
checkouts and unpublished/local Git URLs. Registry dependencies remain pinned by
the root `Cargo.lock`; Nixpkgs remains pinned by `flake.lock`.

| Directory | Upstream | Source commit | License |
| --- | --- | --- | --- |
| `daemon-framework` | https://github.com/pmfleming/daemon-framework | `5b17083569a0bed82c8934ed77494c29a08832bf` | MIT; upstream LICENSE included |
| `shelllist-hyprland` | Shelllist's local `shelllist-hyprland` repository (no remote configured at snapshot time) | `00044fe7517d6632d9cbd9e7aa712aa771f6577d` | MIT, as declared by upstream Cargo.toml; no separate upstream LICENSE file |

The snapshots contain upstream manifests, library source/tests, and README files;
framework workspace metadata and LICENSE are also included. Git metadata, build
outputs, upstream lockfiles and upstream Nix flakes are deliberately excluded.
There are no source modifications in these initial snapshots.

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
