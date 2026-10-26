#!/usr/bin/env python3

"""Exercise the real shell wrapper with an isolated, non-building child process."""

import json
import os
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory
import unittest


SCRIPT = Path(__file__).with_name("run-bazel-ci.sh")
FORK = "CompleteDotTech/codex-subagents-multi-provider"
LICENSE = "BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA"
DECLARATION = f"--repo_env={LICENSE}=1"
TARGET = "//codex-rs/utils/rustls-provider:rustls-provider-preinstalled-test"

STUB = """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

with Path(os.environ["BAZEL_STUB_RECORD"]).open("a", encoding="utf-8") as output:
    output.write(json.dumps({
        "args": sys.argv[1:],
        "environment_declaration": os.environ.get("BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA"),
    }) + "\\n")
if "info" in sys.argv:
    print("/nonexistent-owned-testlogs")
    raise SystemExit(0)
print("stub: no Bazel build or repository access was performed")
raise SystemExit(int(os.environ.get("BAZEL_STUB_EXIT", "0")))
"""


class RunBazelCiTest(unittest.TestCase):
    def invoke(self, *, args=(), flags=(), env=None, targets=(TARGET,)):
        bash = (
            str(
                Path(os.environ.get("ProgramFiles", "C:/Program Files"))
                / "Git/bin/bash.exe"
            )
            if os.name == "nt"
            else shutil.which("bash")
        )
        if not bash:
            self.fail("These shell command tests require Bash")
        with TemporaryDirectory(prefix="bazel-ci-command-") as directory:
            root = Path(directory)
            script = root / SCRIPT.name
            script.write_text(
                SCRIPT.read_text(encoding="utf-8"), encoding="utf-8", newline="\n"
            )
            stub = root / "run_bazel_with_buildbuddy.py"
            stub.write_text(STUB, encoding="utf-8", newline="\n")
            stub.chmod(0o755)
            record = root / "calls.jsonl"
            # Only shell/Python discovery and normal temporary-directory settings
            # survive; no account, provider, or remote-execution credentials do.
            environment = {
                name: value
                for name, value in os.environ.items()
                if name in ("PATH", "SYSTEMROOT", "SystemRoot", "TEMP", "TMP", "TMPDIR")
            }
            environment.update(
                RUNNER_OS="Windows",
                GITHUB_ACTIONS="true",
                GITHUB_REPOSITORY=FORK,
                CODEX_BAZEL_WINDOWS_PATH=r"C:\Program Files\SDK;C:\Windows\System32",
                BAZEL_STUB_RECORD=str(record),
            )
            environment.update(env or {})
            result = subprocess.run(
                [bash, str(script), *flags, "--", "test", *args, "--", *targets],
                cwd=root,
                env=environment,
                capture_output=True,
                text=True,
                check=False,
                timeout=15,
            )
            calls = (
                [json.loads(line) for line in record.read_text().splitlines()]
                if record.exists()
                else []
            )
            return result, calls

    def successful_call(self, **kwargs):
        result, calls = self.invoke(**kwargs)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1, calls)
        return result, calls[0]["args"]

    def test_keyless_windows_cross_uses_coherent_native_sdk_recipe(self):
        result, args = self.successful_call(flags=("--windows-cross-compile",))
        self.assertIn("--config=ci-windows-msvc", args)
        self.assertIn("--platforms=//:windows_x86_64_msvc", args)
        self.assertIn("--host_platform=//:local_windows_msvc", args)
        self.assertNotIn("--config=ci-windows-cross", args)
        self.assertEqual(args.count(DECLARATION), 1)
        self.assertIn("GNU cross compilation is not exercised", result.stdout)
        self.assertEqual(args[args.index("--") + 1 :], [TARGET])

    def test_authenticated_windows_cross_keeps_rbe_and_gnu_route(self):
        _, args = self.successful_call(
            flags=("--windows-cross-compile",),
            env={"BUILDBUDDY_API_KEY": "synthetic-command-test-key"},
        )
        self.assertIn("--config=ci-windows-cross", args)
        self.assertIn("--host_platform=//:rbe", args)
        self.assertIn("--shell_executable=/bin/bash", args)
        self.assertNotIn("--config=ci-windows-msvc", args)
        self.assertNotIn("--platforms=//:windows_x86_64_msvc", args)
        self.assertIn("--action_env=PATH=/usr/bin:/bin", args)
        self.assertIn(DECLARATION, args)

    def test_explicit_matching_platform_choices_remain_unchanged(self):
        for platform in ("//:local_windows_msvc", "//:windows_x86_64_msvc"):
            for separate in (False, True):
                with self.subTest(platform=platform, separate=separate):
                    original = (
                        ("--platforms", platform, "--host_platform", platform)
                        if separate
                        else (f"--platforms={platform}", f"--host_platform={platform}")
                    )
                    _, args = self.successful_call(
                        flags=("--windows-cross-compile",), args=original
                    )
                    self.assertEqual(args[2 : 2 + len(original)], list(original))
                    self.assertEqual(
                        sum(arg.startswith("--platforms") for arg in args), 1
                    )
                    self.assertEqual(
                        sum(arg.startswith("--host_platform") for arg in args), 1
                    )

    def test_unsupported_explicit_platforms_fail_before_child_contact(self):
        for option in ("--platforms", "--host_platform"):
            for platform in ("//:windows_x86_64_gnullvm", "//:custom", ""):
                for separate in (False, True):
                    with self.subTest(
                        option=option, platform=platform, separate=separate
                    ):
                        args = (
                            (option, platform)
                            if separate
                            else (f"{option}={platform}",)
                        )
                        result, calls = self.invoke(
                            flags=("--windows-cross-compile",), args=args
                        )
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(calls, [])
                        self.assertIn("unsupported", result.stderr)

    def test_missing_platform_value_fails_before_child_contact(self):
        result, calls = self.invoke(
            flags=("--windows-cross-compile",), args=("--platforms",)
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("requires a platform value", result.stderr)

    def test_authenticated_explicit_gnu_platform_is_not_replaced(self):
        _, args = self.successful_call(
            flags=("--windows-cross-compile",),
            args=("--platforms=//:windows_x86_64_gnullvm",),
            env={"BUILDBUDDY_API_KEY": "synthetic-command-test-key"},
        )
        self.assertIn("--platforms=//:windows_x86_64_gnullvm", args)
        self.assertNotIn("--config=ci-windows-msvc", args)

    def test_license_declaration_is_scoped_to_authorized_fork_windows_ci(self):
        for env in (
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"GITHUB_REPOSITORY": "another-fork/codex"},
            {"GITHUB_ACTIONS": "false"},
            {"GITHUB_ACTIONS": ""},
            {"RUNNER_OS": "Linux"},
            {"RUNNER_OS": "macOS"},
        ):
            with self.subTest(env=env):
                _, args = self.successful_call(env=env)
                self.assertFalse(any(LICENSE in arg for arg in args))

    def test_explicit_license_declaration_and_opt_out_are_not_overridden(self):
        for value in (LICENSE, f"{LICENSE}=0", f"{LICENSE}=1"):
            for separate in (False, True):
                with self.subTest(value=value, separate=separate):
                    original = (
                        ("--repo_env", value) if separate else (f"--repo_env={value}",)
                    )
                    _, args = self.successful_call(args=original)
                    self.assertEqual(args[2 : 2 + len(original)], list(original))
                    self.assertEqual(sum(LICENSE in arg for arg in args), 1)

    def test_declaration_does_not_modify_process_environment(self):
        result, calls = self.invoke(flags=("--windows-cross-compile",))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(calls[0]["environment_declaration"])
        self.assertIn(DECLARATION, calls[0]["args"])

    def test_child_failure_status_propagates_even_with_diagnostic_output(self):
        result, calls = self.invoke(
            flags=("--windows-cross-compile", "--print-failed-action-summary"),
            env={"BAZEL_STUB_EXIT": "37"},
        )
        self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("--config=ci-windows-msvc", calls[0]["args"])

    def test_opt_out_still_preserves_child_failure(self):
        result, calls = self.invoke(
            flags=("--windows-cross-compile",),
            args=(f"--repo_env={LICENSE}=0",),
            env={"BAZEL_STUB_EXIT": "38"},
        )
        self.assertEqual(result.returncode, 38, result.stderr)
        self.assertNotIn(DECLARATION, calls[0]["args"])

    def test_failed_test_log_lookup_retains_native_platform(self):
        result, calls = self.invoke(
            flags=("--windows-cross-compile", "--print-failed-test-logs"),
            env={"BAZEL_STUB_EXIT": "39"},
        )
        self.assertEqual(result.returncode, 39, result.stdout + result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertIn("info", calls[1]["args"])
        self.assertIn("--platforms=//:windows_x86_64_msvc", calls[1]["args"])
        self.assertIn("--host_platform=//:local_windows_msvc", calls[1]["args"])

    def test_failed_log_lookup_preserves_explicit_platform_aliases(self):
        result, calls = self.invoke(
            flags=("--windows-cross-compile", "--print-failed-test-logs"),
            args=(
                "--platforms",
                "//:local_windows_msvc",
                "--host_platform=//:windows_x86_64_msvc",
            ),
            env={"BAZEL_STUB_EXIT": "40"},
        )
        self.assertEqual(result.returncode, 40, result.stdout + result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertIn("--platforms=//:local_windows_msvc", calls[1]["args"])
        self.assertIn("--host_platform=//:windows_x86_64_msvc", calls[1]["args"])

    def test_authenticated_upstream_does_not_receive_fork_declaration(self):
        _, args = self.successful_call(
            flags=("--windows-cross-compile",),
            env={
                "BUILDBUDDY_API_KEY": "synthetic-command-test-key",
                "GITHUB_REPOSITORY": "openai/codex",
            },
        )
        self.assertIn("--config=ci-windows-cross", args)
        self.assertNotIn(DECLARATION, args)

    def test_target_payload_does_not_change_platform_or_credential_scope(self):
        target_payload = (TARGET, "--platforms=//:windows_x86_64_gnullvm")
        _, args = self.successful_call(
            flags=("--windows-cross-compile",), targets=target_payload
        )
        self.assertIn("--platforms=//:windows_x86_64_msvc", args[: args.index("--")])
        self.assertEqual(args[args.index("--") + 1 :], list(target_payload))


if __name__ == "__main__":
    unittest.main()
