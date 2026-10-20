#!/usr/bin/env bash
# Local co-development gate: requires the live sibling framework and RQLens.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 -m unittest discover -s scripts -p test_quality_regression.py
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo doc --locked --all-features --no-deps
python3 scripts/quality_regression.py "$@"
