"""Adversarial fixtures for configured evidence, without invoking Bazel."""

import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

import voice_configured_metadata as m


def graph():
    return {
        "pathFragments": [
            {"id": 1, "label": "root"},
            {"id": 2, "label": "in", "parentId": 1},
            {"id": 3, "label": "out", "parentId": 1},
        ],
        "artifacts": [{"id": 10, "pathFragmentId": 2}, {"id": 11, "pathFragmentId": 3}],
        "depSetOfFiles": [
            {"id": 20, "directArtifactIds": [10]},
            {"id": 21, "transitiveDepSetIds": [20]},
        ],
        "targets": [{"id": 30, "label": "//fixture:tool", "ruleClassId": 40}],
        "ruleClasses": [{"id": 40, "name": "fixture"}],
        "configuration": [{"id": 50, "mnemonic": "fixture"}],
        "actions": [
            {
                "targetId": 30,
                "configurationId": 50,
                "mnemonic": "Fixture",
                "inputDepSetIds": [21],
                "outputIds": [11],
                "primaryOutputId": 11,
                "arguments": ["compiler.exe", "@root/params"],
                "paramFiles": [{"execPath": "root/params", "arguments": ["root/in"]}],
                "environmentVariables": [{"key": "PATH", "value": "private-value"}],
            }
        ],
    }


def packages():
    records = {
        label: {
            "label": label,
            "provider_keys": ["DefaultInfo"],
            "default": {
                "files": [],
                "executable": None,
                "default_runfiles": [],
                "data_runfiles": [],
            },
        }
        for label in m.LABELS
    }
    actions = []
    for raw_name, package_name in ((m.NAMES[1], m.NAMES[2]), (m.NAMES[3], m.NAMES[4])):
        raw, package = (records[m.PREFIX + name] for name in (raw_name, package_name))
        raw_exe, exe = raw_name + ".exe", package_name + "/tool.exe"
        dlls = [package_name + "/" + str(i) + ".dll" for i in range(25)]
        receipt = package_name + "/packaging.json"
        raw["default"]["executable"] = raw_exe
        raw["default"]["default_runfiles"] = ["upstream"]
        package["default"] = {
            "files": [exe],
            "executable": exe,
            "default_runfiles": dlls + [receipt, "upstream"],
            "data_runfiles": dlls + [receipt],
        }
        package["voice_package"] = {
            "raw_executable": raw_exe,
            "executable": exe,
            "dlls": dlls,
            "receipt": receipt,
        }
        actions.append(
            {
                "owner": package["label"],
                "mnemonic": "VoiceExecutablePackage",
                "inputs": [raw_exe],
                "outputs": [exe, receipt] + dlls,
            }
        )
    records[m.LABELS[0]]["default"]["executable"] = records[m.LABELS[2]][
        "voice_package"
    ]["executable"]
    return list(records.values()), actions


class ParsingTests(unittest.TestCase):
    def test_merged_wrapper_output_and_nested_braces(self):
        value = {"actions": [{"arguments": ["{literal}", "x"]}]}
        text = "BuildBuddy API key is not available; using local Bazel configuration.\n"
        text += json.dumps(value, indent=2) + "\nINFO: done\n"
        self.assertEqual(m.parse_json(text), value)

    def test_truncated_malformed_multiple_and_error_output(self):
        for text in (
            '{"actions": [',
            "{bad}",
            "{}\n{}",
            "ERROR: formatter failed\n{}",
            "{}\nERROR: action failed",
            "notice containing { not JSON\n{}",
        ):
            with self.subTest(text=text), self.assertRaises(m.EvidenceError):
                m.parse_json(text)

    def test_provider_sentinel_exactly_once_and_correct_identity(self):
        record = packages()[0][0]
        text = m.SENTINEL + json.dumps(record) + "\nINFO: done"
        self.assertEqual(m.parse_provider(text, record["label"]), record)
        for wrong in (
            text + "\n" + text,
            "ERROR: failed\n" + text,
            m.SENTINEL + "{}",
            m.SENTINEL + "{",
        ):
            with self.subTest(text=wrong), self.assertRaises(m.EvidenceError):
                m.parse_provider(wrong, record["label"])


class GraphTests(unittest.TestCase):
    def test_complete_graph_paths_and_arguments_retained_env_values_removed(self):
        raw = graph()
        sanitized, resolved = m.graph_metadata(raw)
        self.assertEqual(resolved[0]["inputs"], ["root/in"])
        self.assertEqual(resolved[0]["outputs"], ["root/out"])
        self.assertEqual(
            sanitized["actions"][0]["arguments"], raw["actions"][0]["arguments"]
        )
        self.assertEqual(sanitized["artifacts"], raw["artifacts"])
        self.assertNotIn("private-value", json.dumps(sanitized))
        self.assertIn("private-value", json.dumps(raw))

    def test_missing_references_duplicates_and_cycles_fail(self):
        mutations = [
            lambda g: g["artifacts"][0].update(pathFragmentId=999),
            lambda g: g["actions"][0].update(outputIds=[999]),
            lambda g: g["depSetOfFiles"][0].update(directArtifactIds=[999]),
            lambda g: g["depSetOfFiles"][0].update(transitiveDepSetIds=[21]),
            lambda g: g["pathFragments"][0].update(parentId=2),
            lambda g: g["targets"][0].update(ruleClassId=999),
            lambda g: g["actions"][0].update(configurationId=999),
            lambda g: g["actions"][0].update(aspectDescriptorIds=[999]),
            lambda g: g["artifacts"].append(copy.deepcopy(g["artifacts"][0])),
        ]
        for mutate in mutations:
            value = graph()
            mutate(value)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.graph_metadata(value)

    def test_provider_environment_values_are_private_and_equalities_use_raw_values(
        self,
    ):
        records, actions = packages()
        for record in records[1:3]:
            record["run_environment"] = {
                "environment": {"PATH": "private-path"},
                "inherited_environment_names": [],
            }
        sanitized = m.sanitize_provider(records[1])
        self.assertNotIn("private-path", json.dumps(sanitized))
        self.assertEqual(len(m.package_proof(records, actions)), 2)
        records[2]["run_environment"]["environment"]["PATH"] = "different-private-path"
        with self.assertRaises(m.EvidenceError):
            m.package_proof(records, actions)
        records[1]["run_environment"]["environment"]["UNREVIEWED"] = "secret-value"
        with self.assertRaises(m.EvidenceError):
            m.sanitize_provider(records[1])

    def test_unknown_fields_env_and_secret_arguments_fail(self):
        mutations = [
            lambda g: g.update(unknown=[]),
            lambda g: g["actions"][0].update(unknown="x"),
            lambda g: g["actions"][0]["environmentVariables"].append(
                {"key": "UNREVIEWED", "value": "x"}
            ),
            lambda g: g["actions"][0]["arguments"].append(
                "--remote_header=authorization=x"
            ),
            lambda g: g["actions"][0]["paramFiles"][0]["arguments"].append(
                "--env=PATH=private"
            ),
        ]
        for mutate in mutations:
            value = graph()
            mutate(value)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.graph_metadata(value)

    def test_package_counts_bind_to_actual_provider_paths(self):
        records, actions = packages()
        self.assertEqual(len(m.package_proof(records, actions)), 2)
        for mutate in (
            lambda r, a: a.append(copy.deepcopy(a[0])),
            lambda r, a: a[0]["outputs"].pop(),
            lambda r, a: a[0]["outputs"].__setitem__(0, "unrelated.exe"),
            lambda r, a: a[0]["inputs"].clear(),
            lambda r, a: r[2]["default"]["default_runfiles"].remove("upstream"),
            lambda r, a: r[1].update(run_environment={"environment": {"PATH": "raw"}}),
        ):
            r, a = copy.deepcopy((records, actions))
            mutate(r, a)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.package_proof(r, a)


class CaptureTests(unittest.TestCase):
    def test_oversize_is_failure_without_truncated_success(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(m, "LIMIT", 64),
        ):
            result = m.capture(
                [sys.executable, "-c", 'print("x" * 1000)'],
                Path(directory) / "raw",
                [0],
            )
            self.assertTrue(result["oversize"])
            self.assertLessEqual((Path(directory) / "raw").stat().st_size, 64)

    def test_total_limit_and_nonzero_exit_are_observed(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(m, "TOTAL", 64),
        ):
            result = m.capture(
                [sys.executable, "-c", 'print("x" * 60)'], Path(directory) / "raw", [63]
            )
            self.assertTrue(result["oversize"])
        with tempfile.TemporaryDirectory() as directory:
            result = m.capture(
                [sys.executable, "-c", "raise SystemExit(7)"],
                Path(directory) / "raw",
                [0],
            )
            self.assertEqual(result["exit_code"], 7)


if __name__ == "__main__":
    unittest.main()
