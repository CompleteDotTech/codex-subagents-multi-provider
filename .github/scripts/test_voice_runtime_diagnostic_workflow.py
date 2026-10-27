"""Structural checks for the deliberately isolated manual diagnostic route."""

from pathlib import Path
import re
import os
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
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
        self.assertEqual(
            set(self.jobs),
            ordinary
            | {
                "voice-runtime-diagnostic",
                "pty-runtime-diagnostic",
                "invalid-diagnostic-selection",
            },
        )
        for name in ordinary:
            gate = re.search(r"^    if: (.+)$", self.jobs[name], re.M).group(1)
            self.assertIn(
                "!(github.event_name == 'workflow_dispatch' && (inputs.voice_runtime_diagnostic || inputs.pty_runtime_diagnostic))",
                gate,
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
        self.assertIn("'pty-diagnostic'", self.text.split("jobs:", 1)[0])

    def evaluate(self, expression, event, voice, pty, ref="refs/heads/main"):
        expression = expression.removeprefix("${{").removesuffix("}}").strip()
        expression = expression.replace("&&", " and ").replace("||", " or ")
        expression = re.sub(r"!(?!=)", " not ", expression)
        return eval(
            expression.strip(),
            {"__builtins__": {}},
            {
                "github": SimpleNamespace(event_name=event, ref=ref),
                "inputs": SimpleNamespace(
                    voice_runtime_diagnostic=voice, pty_runtime_diagnostic=pty
                ),
                "always": lambda: True,
            },
        )

    def test_actual_gate_expressions_and_concurrency_cover_all_modes(self):
        ordinary = {
            "test",
            "test-windows-shard",
            "test-windows",
            "test-windows-native-main",
            "clippy",
            "verify-release-build",
        }
        suffix = self.text.split("jobs:", 1)[0].rsplit("::${{", 1)[1].split("}}", 1)[0]
        for event in ("workflow_dispatch", "workflow_call", "pull_request", "push"):
            for voice in (False, True):
                for pty in (False, True):
                    with self.subTest(event=event, voice=voice, pty=pty):
                        manual = event == "workflow_dispatch"
                        for name, body in self.jobs.items():
                            gate = re.search(r"^    if: (.+)$", body, re.M).group(1)
                            actual = bool(self.evaluate(gate, event, voice, pty))
                            if name in ordinary:
                                expected = not (manual and (voice or pty))
                                if name == "test-windows-native-main":
                                    expected = expected and event in (
                                        "push",
                                        "workflow_dispatch",
                                    )
                            elif name == "voice-runtime-diagnostic":
                                expected = manual and voice and not pty
                            elif name == "pty-runtime-diagnostic":
                                expected = manual and pty and not voice
                            else:
                                expected = manual and voice and pty
                            self.assertEqual(actual, expected, name)
                        mode = (
                            (
                                "invalid-diagnostic"
                                if voice and pty
                                else "pty-diagnostic"
                                if pty
                                else "voice-diagnostic"
                                if voice
                                else "normal"
                            )
                            if manual
                            else "normal"
                        )
                        self.assertEqual(self.evaluate(suffix, event, voice, pty), mode)
        invalid = self.jobs["invalid-diagnostic-selection"]
        self.assertIn("exit 1", invalid)
        self.assertNotIn("uses:", invalid)
        for name in ("voice_runtime_diagnostic", "pty_runtime_diagnostic"):
            field = re.search(
                r"^      " + name + r":\n(.*?)(?=^      [a-z_]+:|^concurrency:)",
                self.text,
                re.M | re.S,
            ).group(1)
            self.assertIn("type: boolean", field)
            self.assertIn("default: false", field)

    def test_pty_scope_keeps_original_parallel_suite_and_bounded_text(self):
        job = self.jobs["pty-runtime-diagnostic"]
        self.assertEqual(
            re.findall(r"//codex-rs/[^\s]+", job),
            ["//codex-rs/utils/pty:pty-unit-tests"],
        )
        for value in (
            "runs-on: windows-2025",
            "contents: read",
            "persist-credentials: false",
            "remote-credentials: false",
            "native-windows-voice: true",
            "unset BUILDBUDDY_API_KEY",
            "--nocache_test_results",
            "--test_arg=--nocapture",
            "--windows-cross-compile --remote-download-toplevel",
            "head -c 8388608",
            "test.log test.xml",
            "retention-days: 7",
        ):
            self.assertIn(value, job)
        for value in (
            "secrets.",
            "--test-threads",
            "RUST_TEST_THREADS",
            "--test_timeout",
            "--runs_per_test",
            "--flaky_test_attempts",
            "--build_event_json_file",
            "--keep_going",
            "windows-resource-observation",
        ):
            self.assertNotIn(value, job)
        self.assertIn("Write-Output $json", job)
        self.assertLess(job.index("Write-Output $json"), job.index("Set-Content"))

    def test_actual_pty_collector_missing_exact_and_oversized_files(self):
        job = self.jobs["pty-runtime-diagnostic"]
        collector = re.search(
            r"- name: Collect bounded PTY text evidence\n.*?        run: \|\n(.*?)(?=      - name:)",
            job,
            re.S,
        ).group(1)
        script = "\n".join(line[10:] for line in collector.splitlines())
        bash = (
            r"C:\Program Files\Git\bin\bash.exe"
            if os.name == "nt"
            else shutil.which("bash")
        )
        self.assertTrue(Path(bash).is_file())
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            env = dict(os.environ, RUNNER_TEMP=(root / "output").as_posix())

            def collect():
                subprocess.run(
                    [bash, "-c", script],
                    cwd=root,
                    env=env,
                    check=True,
                    capture_output=True,
                    timeout=15,
                )

            collect()
            output = root / "output/pty-runtime-diagnostic"
            self.assertEqual(list(output.iterdir()), [])
            source = root / "bazel-testlogs/codex-rs/utils/pty/pty-unit-tests"
            source.mkdir(parents=True)
            (source / "test.log").write_bytes(b"original failure transcript")
            (source / "test.xml").write_bytes(b"<testsuite/>")
            collect()
            self.assertEqual(
                (output / "test.log").read_bytes(), b"original failure transcript"
            )
            self.assertEqual((output / "test.xml").read_bytes(), b"<testsuite/>")
            (source / "test.log").write_bytes(b"x" * (8388608 + 1))
            collect()
            self.assertEqual((output / "test.log").stat().st_size, 8388608)
            self.assertIn("8388608", (output / "test.log.truncated.txt").read_text())


if __name__ == "__main__":
    unittest.main()
