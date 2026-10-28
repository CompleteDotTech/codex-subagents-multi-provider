"""Structural checks for the deliberately isolated manual diagnostic route."""

from pathlib import Path
import ast
import itertools
import re
import unittest


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = (
            Path(__file__).resolve().parents[1] / "workflows/bazel.yml"
        ).read_text()
        self.jobs = dict(
            re.findall(
                r"^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)",
                self.text.split("jobs:\n", 1)[1],
                re.M | re.S,
            )
        )

    def test_all_normal_jobs_guarded_and_original_predicates_retained(self):
        ordinary = {
            "plan-package-shards",
            "linux-package-manifest",
            "test",
            "test-windows-shard",
            "test-windows",
            "test-windows-native-main",
            "clippy",
            "verify-release-build",
        }
        self.assertEqual(
            set(self.jobs),
            ordinary
            | {
                "voice-runtime-diagnostic",
                "ownership-fixture-diagnostic",
                "ownership-fixture-full-module-diagnostic",
                "reject-conflicting-diagnostics",
            },
        )
        for name in ordinary:
            gate = re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(1)
            if name == "linux-package-manifest":
                self.assertIn("    needs: plan-package-shards\n", self.jobs[name])
                self.assertIn(
                    "needs.plan-package-shards.outputs.package-shards == 'true'", gate
                )
            self.assertIn(
                "!(github.event_name == 'workflow_dispatch' && (inputs.voice_runtime_diagnostic || inputs.ownership_fixture_diagnostic || inputs.ownership_fixture_full_module_diagnostic))",
                gate,
            )
            # The added conjunction is true for normal dispatch, call, PR, push;
            # false only for manual true mode, preserving prior predicates.
            for event in ["workflow_dispatch", "workflow_call", "pull_request", "push"]:
                for value in [False, None, True]:
                    self.assertEqual(
                        not (event == "workflow_dispatch" and value),
                        not (event == "workflow_dispatch" and value is True),
                    )
        self.assertIn("if: always() && !(", self.jobs["test-windows"])
        self.assertIn(
            "github.event_name == 'push' && github.ref == 'refs/heads/main'",
            self.jobs["test-windows-native-main"],
        )

    def test_diagnostic_scope_and_credentials(self):
        job = self.jobs["voice-runtime-diagnostic"]
        self.assertIn(
            "if: github.event_name == 'workflow_dispatch' && inputs.voice_runtime_diagnostic",
            job,
        )
        self.assertIn("contents: read", job)
        self.assertNotIn("secrets.", job)
        self.assertNotIn("write", job)
        self.assertIn("unset BUILDBUDDY_API_KEY", job)
        self.assertIn("persist-credentials: false", job)
        self.assertIn("native-windows-voice: true", job)
        self.assertIn("--nocache_test_results", job)
        self.assertIn("--keep_going", job)
        self.assertEqual(
            re.findall(r"//codex-rs/[^\s]+", job),
            [
                "//codex-rs/voice-host:voice-host-windows-runtime-diagnostic-test",
                "//codex-rs/voice-host:codex-voice-host-bin-unit-tests",
                "//codex-rs/voice-host:voice-host-installed_client-test",
            ],
        )
        self.assertNotIn("//...", job)
        self.assertIn("'voice-diagnostic' || 'normal'", self.text.split("jobs:", 1)[0])

    @staticmethod
    def evaluate_route_gate(expression, event, voice, ownership, full=False, refname="topic"):
        # Parse ONLY boolean/comparison syntax used by these actual job gates.
        # No eval, function calls or workflow code is executed by the tests.
        expression = expression.replace("github.event_name", "event")
        expression = expression.replace("github.ref_name", "refname")
        expression = expression.replace("inputs.ownership_fixture_full_module_diagnostic", "full")
        expression = expression.replace("inputs.voice_runtime_diagnostic", "voice")
        expression = expression.replace(
            "inputs.ownership_fixture_diagnostic", "ownership"
        )
        expression = (
            expression.replace("&&", " and ")
            .replace("||", " or ")
            .replace("!=", " NOT_EQUAL ")
            .replace("!", "not ")
            .replace("NOT_EQUAL", "!=")
        )
        bindings = {
            "event": event,
            "voice": voice,
            "ownership": ownership,
            "full": full,
            "refname": refname,
        }

        def value(node):
            if isinstance(node, ast.Name):
                return bindings[node.id]
            if isinstance(node, ast.Constant):
                return node.value
            if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
                return not value(node.operand)
            if isinstance(node, ast.BoolOp):
                for item in node.values:
                    result = value(item)
                    if isinstance(node.op, ast.And) and not result:
                        return result
                    if isinstance(node.op, ast.Or) and result:
                        return result
                return result
            if (
                isinstance(node, ast.Compare)
                and len(node.ops) == 1
                and isinstance(node.ops[0], (ast.Eq, ast.NotEq))
            ):
                same = value(node.left) == value(node.comparators[0])
                return same if isinstance(node.ops[0], ast.Eq) else not same
            raise AssertionError("unexpected route expression shape")

        return value(ast.parse(expression.strip(), mode="eval").body)

    def test_actual_manual_route_gates_are_disjoint_and_reject_both_inputs(self):
        route_jobs = {
            "voice-runtime-diagnostic",
            "ownership-fixture-diagnostic",
            "reject-conflicting-diagnostics",
        }
        for event in ("workflow_dispatch", "workflow_call", "pull_request", "push"):
            for voice in (False, None, True):
                for ownership in (False, None, True):
                    actual = {
                        name
                        for name in route_jobs
                        if self.evaluate_route_gate(
                            re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(
                                1
                            ),
                            event,
                            voice,
                            ownership,
                        )
                    }
                    expected = set()
                    if event == "workflow_dispatch":
                        if voice and ownership:
                            expected = {"reject-conflicting-diagnostics"}
                        elif voice:
                            expected = {"voice-runtime-diagnostic"}
                        elif ownership:
                            expected = {"ownership-fixture-diagnostic"}
                    self.assertEqual(actual, expected, (event, voice, ownership))
        self.assertIn("exit 1", self.jobs["reject-conflicting-diagnostics"])
        prefix = self.text.split("jobs:", 1)[0]
        for suffix in (
            "diagnostic-conflict",
            "ownership-diagnostic",
            "voice-diagnostic",
            "normal",
        ):
            self.assertIn("'" + suffix + "'", prefix)

    def test_ownership_module_route_is_bounded_and_has_no_build_or_upload(self):
        job = self.jobs["ownership-fixture-diagnostic"]
        self.assertIn("runs-on: ubuntu-24.04", job)
        self.assertIn("timeout-minutes: 5", job)
        self.assertIn("contents: read", job)
        self.assertIn("actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd", job)
        self.assertIn("persist-credentials: false", job)
        self.assertIn('"$head" != "$GITHUB_SHA"', job)
        self.assertIn("git rev-parse 'HEAD^{tree}'", job)
        self.assertIn("OWNERSHIP_DIAGNOSTIC_HEAD=%s", job)
        self.assertIn("OWNERSHIP_DIAGNOSTIC_TREE=%s", job)
        self.assertIn(
            "run: python3 .github/scripts/diagnose_ownership_fixture_startup.py",
            job,
        )
        self.assertIn("fail-fast: false", job)
        self.assertIn("mode: [minimal, full]", job)
        self.assertIn("OWNERSHIP_STARTUP_MODE: ${{ matrix.mode }}", job)
        for forbidden in (
            "secrets.",
            "prepare-bazel-ci",
            "setup-bazel-ci",
            "setup-python",
            "upload-artifact",
            "continue-on-error",
            "sudo ",
            "//...",
            "--jobs",
            "timeout=",
            "pip install",
        ):
            self.assertNotIn(forbidden, job)
        setup = (Path(__file__).parents[1] / "actions/setup-ci/action.yml").read_text()
        self.assertIn('tmp="$CI_BUILD_ROOT/tmp"', setup)
        self.assertIn('echo "TEMP=$tmp"', setup)
        self.assertIn('echo "TMP=$tmp"', setup)
        self.assertIn('fixture_tmp="$HOME/.cache/codex-ci/tmp"', job)
        self.assertIn('echo "TEMP=$fixture_tmp"', job)
        self.assertIn('echo "TMP=$fixture_tmp"', job)
        self.assertNotIn("setup-python", setup)

    def test_actual_concurrency_suffix_isolates_each_manual_mode_from_normal(self):
        line = re.search(r"^  group: (.+)$", self.text, re.M).group(1)
        expression = line.rsplit("${{", 1)[1].rsplit("}}", 1)[0]
        for event in ("workflow_dispatch", "workflow_call", "pull_request", "push"):
            for voice in (False, None, True):
                for ownership in (False, None, True):
                    expected = "normal"
                    if event == "workflow_dispatch":
                        if voice and ownership:
                            expected = "diagnostic-conflict"
                        elif ownership:
                            expected = "ownership-diagnostic"
                        elif voice:
                            expected = "voice-diagnostic"
                    self.assertEqual(
                        self.evaluate_route_gate(expression, event, voice, ownership),
                        expected,
                    )
        self.assertRegex(
            self.text,
            r"ownership_fixture_diagnostic:\n(?:        [^\n]+\n)*        type: boolean\n        default: false",
        )


    def test_full_module_route_is_single_original_command_without_warmup(self):
        job = self.jobs["ownership-fixture-full-module-diagnostic"]
        command = "python3 -m unittest discover -s .github/scripts -p test_windows_ci_resource_observation.py"
        self.assertEqual(job.count(command), 1)
        self.assertIn("runs-on: ubuntu-24.04", job)
        self.assertIn("timeout-minutes: 5", job)
        self.assertIn("contents: read", job)
        self.assertIn("persist-credentials: false", job)
        self.assertIn('"$head" != "$GITHUB_SHA"', job)
        self.assertIn('fixture_tmp="$HOME/.cache/codex-ci/tmp"', job)
        self.assertIn('echo "TEMP=$fixture_tmp"', job)
        self.assertIn('echo "TMP=$fixture_tmp"', job)
        for forbidden in (
            "strategy:", "OWNERSHIP_STARTUP_MODE",
            "diagnose_ownership_fixture_startup.py", "pwsh ", "powershell ",
            "uses: ./.github/actions/setup-ci", "setup-python", "bazel test",
            "secrets.", "continue-on-error", "upload-artifact", "retry", "warmup.py",
        ):
            self.assertNotIn(forbidden, job)

    def test_three_manual_choices_are_exclusive_and_guard_all_normal_jobs(self):
        routes = {
            "voice-runtime-diagnostic", "ownership-fixture-diagnostic",
            "ownership-fixture-full-module-diagnostic", "reject-conflicting-diagnostics",
        }
        for event in ("workflow_dispatch", "workflow_call", "pull_request", "push"):
            for voice, ownership, full in itertools.product((False, None, True), repeat=3):
                actual = {
                    name for name in routes
                    if self.evaluate_route_gate(
                        re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(1),
                        event, voice, ownership, full,
                    )
                }
                selected = sum(bool(value) for value in (voice, ownership, full))
                expected = set()
                if event == "workflow_dispatch" and selected:
                    if selected > 1:
                        expected = {"reject-conflicting-diagnostics"}
                    elif voice:
                        expected = {"voice-runtime-diagnostic"}
                    elif ownership:
                        expected = {"ownership-fixture-diagnostic"}
                    else:
                        expected = {"ownership-fixture-full-module-diagnostic"}
                self.assertEqual(actual, expected, (event, voice, ownership, full))
        for name in set(self.jobs) - routes:
            gate = re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(1)
            self.assertIn("inputs.ownership_fixture_full_module_diagnostic", gate)

    def test_manual_full_module_never_cancels_existing_runs(self):
        prefix = self.text.split("jobs:", 1)[0]
        self.assertIn("(github.ref_name == 'main' || (github.event_name == 'workflow_dispatch' && inputs.ownership_fixture_full_module_diagnostic)) && format('::{0}', github.run_id)", prefix)
        self.assertIn("'ownership-full-module'", prefix)
        cancel = re.search(
            r"^  cancel-in-progress: \$\{\{ (.*?) \}\}$", prefix, re.M
        ).group(1)
        for event in ("workflow_dispatch", "workflow_call", "pull_request", "push"):
            for refname, full in itertools.product(("main", "topic"), (False, None, True)):
                actual = self.evaluate_route_gate(cancel, event, False, False, full, refname)
                self.assertEqual(
                    actual,
                    refname != "main" and not (event == "workflow_dispatch" and bool(full)),
                )

if __name__ == "__main__":
    unittest.main()
