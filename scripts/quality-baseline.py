#!/usr/bin/env python3
"""Export a compact, reviewable baseline from one complete RQLens measurement run."""
import argparse
import collections
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = (
    "hotspots", "clones", "rust_escape_hatches", "reliability_findings",
    "locality_metrics", "leverage_metrics", "coverage", "rust_practices",
    "architecture_rules", "test_quality", "api_health", "semantic_api",
    "type_health", "correctness_review", "function_risk", "map", "module_cohesion",
)


def production(row):
    path = Path(row["path"]).relative_to(ROOT)
    return (path.parts[0] == "src" and "benchmarks" not in path.stem
            and "tests" not in path.stem and path.stem != "test_provider"
            and "tests" not in path.parts and "::tests::" not in row["name"])


def complexity(functions):
    return {
        "function_count": len(functions),
        "cognitive_sum": sum(row["cognitive_complexity"] for row in functions),
        "cognitive_max": max(row["cognitive_complexity"] for row in functions),
        "cyclomatic_sum": sum(row["cyclomatic_complexity"] for row in functions),
        "cyclomatic_max": max(row["cyclomatic_complexity"] for row in functions),
        "hotspot_max": max(row["score"] for row in functions),
        "top_hotspots": [
            {key: row[key] for key in ("name", "score", "sloc", "cognitive_complexity", "cyclomatic_complexity")}
            for row in sorted(functions, key=lambda row: row["score"], reverse=True)[:5]
        ],
    }


def export(directory):
    artifacts = {name: json.loads((directory / f"{name}.json").read_text()) for name in ARTIFACTS}
    reference = artifacts["hotspots"]
    for name, artifact in artifacts.items():
        if not artifact["measurement_confidence"]["complete"]:
            raise ValueError(f"{name}: incomplete measurement; baseline not updated")
        if artifact["input_fingerprint"] != reference["input_fingerprint"]:
            raise ValueError(f"{name}: mixed/stale measurement inputs; baseline not updated")
    if artifacts["rust_practices"]["summary"]["failed_errors"]:
        raise ValueError("verification errors; baseline not updated")
    policy = json.loads((directory / "policy_report.json").read_text())
    required_policies = {"partial", "test-failure", "practice-failure"}
    if not policy["passed"] or not required_policies.issubset(policy["enabled_policies"]):
        raise ValueError("run successful completeness, test, and practice policy checks first")
    functions = [row for row in reference["records"] if row["kind"] == "function"]
    locality = {row["module_key"]: row for row in artifacts["locality_metrics"]["records"]}
    leverage = {row["module_key"]: row for row in artifacts["leverage_metrics"]["records"]}
    warnings = [row["rule_id"] for row in artifacts["rust_practices"]["data"]["checks"] if row["status"] == "failed"]
    return {
        "schema_version": 1,
        "generator_version": reference["generator_version"],
        "generated_at": max(artifact["generated_at"] for artifact in artifacts.values()),
        "risk_model": {"id": reference["risk_model_id"], "version": reference["risk_model_version"]},
        "measured_input": reference["input_fingerprint"],
        "all_code": complexity(functions),
        "production_functions": complexity([row for row in functions if production(row)]),
        "architecture": {
            "mean_locality": sum(row["locality_score"] for row in locality.values()) / len(locality),
            "mean_leverage": sum(row["leverage_score"] for row in leverage.values()) / len(leverage),
            "modules": {name: {
                "locality": row["locality_score"], "leverage": leverage[name]["leverage_score"],
                "inbound": row["inbound_dependencies"], "outbound": row["outbound_dependencies"],
            } for name, row in sorted(locality.items())},
        },
        "clones": artifacts["clones"]["summary"],
        "escape_hatches": artifacts["rust_escape_hatches"]["summary"],
        "reliability_scopes": dict(collections.Counter(row["scope"] for row in artifacts["reliability_findings"]["records"])),
        "coverage": artifacts["coverage"]["summary"],
        "coverage_files": {row["path"]: row["lines"] for row in artifacts["coverage"]["data"]["files"]},
        "verification": artifacts["rust_practices"]["summary"],
        "verification_warnings": warnings,
        "test_counts": {key: artifacts["correctness_review"]["summary"][key]
                        for key in ("inline_count", "integration_count", "failed")},
        "policy": {key: policy[key] for key in ("passed", "enabled_policies", "failed_policies", "threshold_violations")},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=ROOT / "target/quality-current")
    parser.add_argument("--output", type=Path, default=ROOT / "quality/baseline.json")
    args = parser.parse_args()
    report = export(args.input)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
