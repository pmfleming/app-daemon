"""Gate contract tests: missing evidence and regressions must never become zeros."""
import copy
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import quality_regression as gate

class RegressionGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.files = {"src/lib.rs": b"fn work() { value.clone(); }\n", "scripts/check.py": b"pass\n"}
        self.policy = {"allowed_partial_patterns": [{"pattern": "known macro", "max_matches": 1, "reason": "authored syntax only"}]}
        state = {"complete": True, "partial": False, "missing_input": [], "stale_input": [], "unsupported_pattern": [],
                 "semantic_identity": {"complete": True, "available": True, "unresolved_count": 0}}
        common = {"schema_version": 3, "generator_version": "fixture", "risk_model_id": "risk", "risk_model_version": 4,
                  "input_fingerprint": {"complete": True, "read_errors": [], "config_digest": "config", "digest": "fresh"},
                  "measurement_confidence": state, "records": [], "summary": {}}
        self.artifacts = {name: copy.deepcopy(common) for name in gate.PRODUCERS.values()}
        function = {"kind": "function", "path": str(self.root / "src/lib.rs"), "name": "work",
                    "cognitive_complexity": 2, "cyclomatic_complexity": 3, "halstead_effort": 100.0,
                    "complexity_metric_model_id": "complexity", "complexity_metric_model_version": 2,
                    "halstead_metric_model_id": "halstead", "halstead_metric_model_version": 1}
        self.artifacts["hotspots"]["records"] = [function, {"kind": "module", "path": function["path"]}]
        self.artifacts["clones"]["summary"] = {"duplicated_line_count": 0}
        self.artifacts["rust_escape_hatches"]["summary"] = {"record_count": 0}
        for name, metric in [("locality_metrics", "locality_score"), ("leverage_metrics", "leverage_score")]:
            self.artifacts[name]["records"] = [{"module_id": "app::lib", metric: 90.0}]

    def snapshot(self):
        for name, artifact in self.artifacts.items():
            (self.root / f"{name}.json").write_text(json.dumps(artifact))
        return gate.snapshot(self.root, self.root, self.files, "instrument", self.policy)

    def test_equal_and_improving_metrics_pass_but_every_regression_blocks(self):
        baseline = self.snapshot()
        self.assertFalse(gate.regressions(baseline, copy.deepcopy(baseline)))
        for key, value in baseline["metrics"].items():
            with self.subTest(key=key):
                changed = copy.deepcopy(baseline)
                changed["metrics"][key] = value + (-1 if key in gate.HIGHER_IS_BETTER else 1)
                self.assertIn(key, gate.regressions(baseline, changed))
                changed["metrics"][key] = value + 1 if key in gate.HIGHER_IS_BETTER else max(0, value - 1)
                self.assertFalse(gate.regressions(baseline, changed))
        self.assertEqual(baseline["metrics"]["rust.all.lines"], 1)
        self.assertEqual(baseline["metrics"]["code.rs_py_sh.lines"], 2)
        self.assertEqual(baseline["metrics"]["rust.all.clone_sites"], 1)

    def test_stale_missing_nonfinite_and_partial_evidence_fail_closed(self):
        original = copy.deepcopy(self.artifacts)
        mutations = [
            lambda a: a["clones"]["input_fingerprint"].update(digest="stale"),
            lambda a: a["clones"]["measurement_confidence"]["missing_input"].append("source"),
            lambda a: a["clones"]["measurement_confidence"]["stale_input"].append("source"),
            lambda a: a["hotspots"]["records"][0].update(halstead_effort=None),
            lambda a: a["hotspots"]["records"][0].update(halstead_effort=float("nan")),
            lambda a: a["hotspots"]["records"][0].update(cognitive_complexity=-1),
            lambda a: a["hotspots"]["records"].pop(),
            lambda a: a["hotspots"].pop("records"),
            lambda a: a["locality_metrics"]["measurement_confidence"]["semantic_identity"].update(complete=False),
            lambda a: a["clones"]["measurement_confidence"].update(complete=False, partial=True, unsupported_pattern=["new macro"]),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                self.artifacts = copy.deepcopy(original)
                mutate(self.artifacts)
                with self.assertRaises((ValueError, KeyError)):
                    self.snapshot()

    def test_cli_requires_review_and_never_silently_rewrites_baseline(self):
        baseline = self.snapshot()
        baseline_path = self.root / "baseline.json"
        baseline_path.write_text(json.dumps(baseline))
        original = baseline_path.read_bytes()
        current = copy.deepcopy(baseline)
        current["metrics"]["rust.all.lines"] += 1
        tool = self.root / "rqlens"
        tool.write_bytes(b"instrument")
        (self.root / "quality").mkdir()
        (self.root / "quality/regression.toml").write_text("project_root = '..'\n")
        (self.root / "quality/regression-policy.json").write_text(json.dumps({"schema_version": 1, **self.policy}))
        args = SimpleNamespace(rqlens=tool, baseline=baseline_path, accept_reason=None)
        with patch.object(gate, "ROOT", self.root), patch.object(gate, "sources", return_value=self.files) as sources, \
                patch.object(gate, "snapshot", return_value=current), patch.object(gate.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "unreviewed regressions"):
                gate.run(args)
            self.assertEqual(baseline_path.read_bytes(), original)
            self.assertEqual([call.args[0][2] for call in run.call_args_list], [*gate.PRODUCERS, "hotspots"])
            sources.side_effect = [self.files, {**self.files, "src/new.rs": b""}]
            with self.assertRaisesRegex(ValueError, "changed during measurement"):
                gate.run(args)
            sources.side_effect = None
            args.accept_reason = " "
            with self.assertRaisesRegex(ValueError, "non-empty review reason"):
                gate.run(args)
            args.accept_reason = "Add a reviewed regression test; keep total code cost visible."
            gate.run(args)
            review = json.loads(baseline_path.read_text())["review"]
            self.assertEqual(review["reason"], args.accept_reason)
            self.assertIn("rust.all.lines", review["accepted_regressions"])
            baseline_path.unlink()
            args.accept_reason = None
            with self.assertRaisesRegex(ValueError, "no baseline"):
                gate.run(args)
            self.assertFalse(baseline_path.exists())

if __name__ == "__main__":
    unittest.main()
