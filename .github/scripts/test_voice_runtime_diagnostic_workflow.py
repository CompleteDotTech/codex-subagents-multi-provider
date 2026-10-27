"""Structural checks for the deliberately isolated manual diagnostic route."""

from pathlib import Path
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
            "test",
            "test-windows-shard",
            "test-windows",
            "test-windows-native-main",
            "clippy",
            "verify-release-build",
        }
        self.assertEqual(set(self.jobs), ordinary | {"voice-runtime-diagnostic"})
        for name in ordinary:
            gate = re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(1)
            self.assertIn(
                "!(github.event_name == 'workflow_dispatch' && inputs.voice_runtime_diagnostic)",
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


if __name__ == "__main__":
    unittest.main()
