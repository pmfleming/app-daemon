#!/usr/bin/env python3
"""Regenerate RQLens evidence and reject unreviewed source-metric regressions."""
import argparse
import hashlib
import json
import math
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRODUCERS = {
    "hotspots": "hotspots",
    "clones": "clones",
    "escape-hatches": "rust_escape_hatches",
    "reliability": "reliability_findings",
    "locality": "locality_metrics",
    "leverage": "leverage_metrics",
}
HIGHER_IS_BETTER = {"architecture.locality_mean", "architecture.leverage_mean"}
EXCLUDED = ("tests", "test_provider", "benchmarks", "fixtures")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        raise ValueError(f"invalid/missing/non-finite metric: {value!r}")
    return value


def production(path, name=""):
    return path.startswith("src/") and not any(part in path for part in EXCLUDED) and "::tests::" not in name


def sources(root):
    names = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.rs", "*.py", "*.sh"],
        cwd=root,
    ).decode().split("\0")
    return {name: (root / name).read_bytes() for name in sorted(set(names) - {""})}


def source_digest(files):
    return digest(json.dumps({name: digest(data) for name, data in files.items()}, sort_keys=True).encode())


def relative(path, root):
    path = Path(path)
    return path.relative_to(root).as_posix() if path.is_absolute() else path.as_posix()


def confidence(artifact, root, policy):
    state = artifact["measurement_confidence"]
    if state["missing_input"] or state["stale_input"]:
        raise ValueError("missing/stale measurement inputs")
    patterns = policy["allowed_partial_patterns"]
    counts = [0] * len(patterns)
    reasons = [reason.replace(str(root) + "/", "") for reason in state["unsupported_pattern"]]
    for reason in reasons:
        matches = [i for i, rule in enumerate(patterns) if re.fullmatch(rule["pattern"], reason)]
        if len(matches) != 1:
            raise ValueError(f"unreviewed partial evidence: {reason}")
        counts[matches[0]] += 1
    for count, rule in zip(counts, patterns):
        if not rule["reason"].strip() or count > rule["max_matches"]:
            raise ValueError("partial-evidence exception exceeded or unjustified")
    if state["complete"] is not (not reasons) or state["partial"] is not bool(reasons):
        raise ValueError("inconsistent/unknown completeness status")
    return reasons


def snapshot(root, directory, files, instrument, policy):
    artifacts = {name: json.loads((directory / f"{name}.json").read_text()) for name in PRODUCERS.values()}
    reference = artifacts["hotspots"]
    fingerprint = reference["input_fingerprint"]
    if fingerprint["complete"] is not True or fingerprint["read_errors"]:
        raise ValueError("incomplete input fingerprint")
    partial = {}
    for name, artifact in artifacts.items():
        if artifact["input_fingerprint"] != fingerprint:
            raise ValueError(f"{name}: mixed/stale fingerprints")
        for field in ("generator_version", "risk_model_id", "risk_model_version"):
            if artifact[field] != reference[field]:
                raise ValueError(f"{name}: mixed instrument/model metadata")
        partial[name] = confidence(artifact, root, policy)
    for name in ("locality_metrics", "leverage_metrics"):
        identity = artifacts[name]["measurement_confidence"]["semantic_identity"]
        if identity["complete"] is not True or identity["available"] is not True or identity["unresolved_count"] != 0:
            raise ValueError(f"{name}: incomplete semantic dependency identity")

    rows = reference["records"]
    rust = {path: data for path, data in files.items() if path.endswith(".rs")}
    inventory = {relative(row["path"], root) for row in rows if row["kind"] == "module"}
    if inventory != rust.keys():
        raise ValueError(f"Rust inventory mismatch: {sorted(inventory ^ rust.keys())}")
    functions = [row for row in rows if row["kind"] == "function"]
    if not functions:
        raise ValueError("empty function inventory")
    metrics = {}
    for scope, selected in (
        ("all", functions),
        ("production", [row for row in functions if production(relative(row["path"], root), row["name"])]),
    ):
        if not selected:
            raise ValueError(f"empty {scope} scope")
        for metric in ("cognitive_complexity", "cyclomatic_complexity", "halstead_effort"):
            values = [number(row[metric]) for row in selected]
            metrics[f"{scope}.{metric}.sum"] = sum(values)
            metrics[f"{scope}.{metric}.max"] = max(values)
    for scope, selected in (
        ("all", rust),
        ("production_files", {path: data for path, data in rust.items() if production(path)}),
    ):
        metrics[f"rust.{scope}.lines"] = sum(len(data.splitlines()) for data in selected.values())
        metrics[f"rust.{scope}.clone_sites"] = sum(len(re.findall(rb"\.\s*(?:clone|cloned)\s*\(", data)) for data in selected.values())
        metrics[f"rust.{scope}.to_owned_sites"] = sum(len(re.findall(rb"\.\s*to_owned\s*\(", data)) for data in selected.values())
    metrics["code.rs_py_sh.lines"] = sum(len(data.splitlines()) for data in files.values())
    metrics["duplicates.lines"] = number(artifacts["clones"]["summary"]["duplicated_line_count"])
    escapes = artifacts["rust_escape_hatches"]
    if escapes["summary"]["record_count"] != len(escapes["records"]):
        raise ValueError("inconsistent escape-hatch inventory")
    metrics["escape_hatches"] = len(escapes["records"])
    metrics["reliability.production_findings"] = sum(row["scope"] == "production" for row in artifacts["reliability_findings"]["records"])

    modules = {}
    for artifact, metric in (("locality_metrics", "locality_score"), ("leverage_metrics", "leverage_score")):
        records = artifacts[artifact]["records"]
        values = {row["module_id"]: number(row[metric]) for row in records}
        if not values or len(values) != len(records):
            raise ValueError("empty/duplicate architecture identities")
        modules[metric] = values
        metrics[f"architecture.{metric.removesuffix('_score')}_mean"] = sum(values.values()) / len(values)
    if modules["locality_score"].keys() != modules["leverage_score"].keys():
        raise ValueError("mismatched architecture inventories")
    models = {}
    for metric in ("complexity", "halstead"):
        versions = {(row[f"{metric}_metric_model_id"], row[f"{metric}_metric_model_version"]) for row in functions}
        if len(versions) != 1:
            raise ValueError("mixed metric models")
        models[metric] = list(versions.pop())
    return {
        "schema_version": 1,
        "scope_model": "app-daemon-authored-rust-v1",
        "instrument": instrument,
        "generator_version": reference["generator_version"],
        "artifact_schemas": {name: value["schema_version"] for name, value in artifacts.items()},
        "risk_model": [reference["risk_model_id"], reference["risk_model_version"]],
        "metric_models": models,
        "config_digest": fingerprint["config_digest"],
        "policy_digest": digest(json.dumps(policy, sort_keys=True).encode()),
        "measured_input": fingerprint,
        "source_digest": source_digest(files),
        "partial_evidence": partial,
        "metrics": metrics,
        "modules": modules,
    }


def regressions(before, after):
    failures = {}
    for key in ("schema_version", "scope_model", "instrument", "generator_version", "artifact_schemas", "risk_model", "metric_models", "config_digest", "policy_digest"):
        if before[key] != after[key]:
            failures[f"incomparable.{key}"] = {"before": before[key], "after": after[key]}
    if before["metrics"].keys() != after["metrics"].keys():
        failures["metric.inventory"] = {"before": sorted(before["metrics"]), "after": sorted(after["metrics"])}
    for key in before["metrics"].keys() & after["metrics"].keys():
        old, new = number(before["metrics"][key]), number(after["metrics"][key])
        worsened = new < old if key in HIGHER_IS_BETTER else new > old
        if worsened and not math.isclose(new, old, rel_tol=0, abs_tol=1e-6):
            failures[key] = {"before": old, "after": new}
    for metric, values in before["modules"].items():
        current = after["modules"][metric]
        if values.keys() != current.keys():
            failures[f"{metric}.inventory"] = {"before": sorted(values), "after": sorted(current)}
        for module in values.keys() & current.keys():
            old, new = number(values[module]), number(current[module])
            if new < old and not math.isclose(new, old, rel_tol=0, abs_tol=1e-6):
                failures[f"{module}.{metric}"] = {"before": old, "after": new}
    return dict(sorted(failures.items()))


def run(args):
    if args.accept_reason is not None and not args.accept_reason.strip():
        raise ValueError("baseline acceptance requires a non-empty review reason")
    instrument = args.rqlens.resolve()
    tool_hash = digest(instrument.read_bytes())
    config = ROOT / "quality/regression.toml"
    policy_path = ROOT / "quality/regression-policy.json"
    inputs = {path: path.read_bytes() for path in (config, policy_path)}
    policy = json.loads(inputs[policy_path])
    if policy["schema_version"] != 1:
        raise ValueError("unsupported partial-evidence policy")
    files = sources(ROOT)
    # The last extraction detects input changes between producers, including siblings.
    for producer in [*PRODUCERS, "hotspots"]:
        subprocess.run([str(instrument), "measure", producer, "--config", str(config)], cwd=ROOT, check=True)
    if (files != sources(ROOT) or tool_hash != digest(instrument.read_bytes())
            or any(data != path.read_bytes() for path, data in inputs.items())):
        raise ValueError("source/instrument/policy/config changed during measurement")
    report = snapshot(ROOT, ROOT / "target/quality-regression", files, tool_hash, policy)
    baseline = args.baseline.resolve()
    previous = json.loads(baseline.read_text()) if baseline.exists() else None
    failures = regressions(previous, report) if previous else {}
    if args.accept_reason is not None:
        report["review"] = {
            "reason": args.accept_reason.strip(),
            "accepted_at": datetime.now(timezone.utc).isoformat(),
            "previous_source_digest": previous["source_digest"] if previous else None,
            "accepted_regressions": failures,
        }
        baseline.parent.mkdir(parents=True, exist_ok=True)
        baseline.write_text(json.dumps(report, indent=2, sort_keys=True, allow_nan=False) + "\n")
        print(f"Reviewed baseline written to {baseline}; commit it with the justification.")
    elif previous is None:
        raise ValueError("no baseline: initialize explicitly with --accept-reason")
    elif failures:
        print(json.dumps(failures, indent=2), file=sys.stderr)
        raise ValueError("unreviewed regressions; fix them or use --accept-reason with an explicit review")
    else:
        print("Quality regression gate passed (authored syntax only; known partial inventory remains explicit).")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rqlens", type=Path, default=ROOT.parent / "rust-quality-lens/target/debug/rqlens")
    parser.add_argument("--baseline", type=Path, default=ROOT / "quality/regression-baseline.json")
    parser.add_argument("--accept-reason", help="Deliberately replace the baseline and record every accepted regression")
    args = parser.parse_args()
    try:
        run(args)
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"quality gate: {error}\n")


if __name__ == "__main__":
    main()
