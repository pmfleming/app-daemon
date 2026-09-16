# Vendored Hyprland library

Only `shelllist-hyprland` remains vendored. Its source commit is
`61376dd84a0a8f5edef2da02b11b36cd0e3edbf4` from the local Shelllist Hyprland
repository, licensed MIT as declared by its Cargo manifest. No separate upstream
LICENSE file existed at snapshot time. The snapshot includes phase 4's typed
work-area parser, workspace rule resolution, reply-size bound and regression tests.

To update it, export the same paths from a reviewed upstream commit, update this
provenance, and run Cargo tests/Clippy plus the current-sibling Shelllist matrix.

**Do not vendor daemon-framework.** All five daemons must use the same current
`../daemon-framework` source. Nix builds use
`python3 ../daemon-framework/tools/local-build.py build .`; Cargo uses sibling
path dependencies. Framework changes must exercise all consumers immediately,
not wait for a snapshot refresh or deployment-lock update.
