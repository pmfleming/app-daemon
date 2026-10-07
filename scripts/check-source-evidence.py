#!/usr/bin/env python3
"""Regenerate and require complete, matching RQLens source evidence (not compiled type counts)."""
import argparse
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PRODUCERS = {
    "hotspots": "hotspots",
    "clones": "clones",
    "escape-hatches": "rust_escape_hatches",
    "reliability": "reliability_findings",
    "locality": "locality_metrics",
    "leverage": "leverage_metrics",
    "api-health": "api_health",
}


def validate(directory):
    fingerprint = None
    for name in PRODUCERS.values():
        path = directory / f"{name}.json"
        artifact = json.loads(path.read_text())
        if artifact["measurement_confidence"]["complete"] is not True:
            raise ValueError(f"{path}: incomplete source evidence")
        current = artifact["input_fingerprint"]
        if current["complete"] is not True:
            raise ValueError(f"{path}: incomplete input fingerprint")
        if fingerprint is not None and current != fingerprint:
            raise ValueError(f"{path}: mixed/stale input fingerprints")
        fingerprint = current


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rqlens", type=Path,
                        default=ROOT.parent / "rust-quality-lens/target/debug/rqlens")
    args = parser.parse_args()
    config_path = ROOT / "rqlens.toml"
    config = tomllib.loads(config_path.read_text())
    for tool in PRODUCERS:
        subprocess.run([str(args.rqlens.resolve()), "measure", tool,
                        "--config", str(config_path)], cwd=ROOT, check=True)
    directory = ROOT / config["project_root"] / config["output_dir"]
    validate(directory)
    print("Source evidence complete and fingerprint-consistent; type/coverage/policy gates not asserted.")


if __name__ == "__main__":
    main()
