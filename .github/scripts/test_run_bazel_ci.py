#!/usr/bin/env python3

"""Exercise the real shell wrapper with an isolated, non-building child process."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
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
    def test_native_fork_binds_actual_workspace_status_producer(self):
        _, args = self.successful_call(flags=("--windows-cross-compile",))
        self.assertEqual(
            args.count("--workspace_status_command=scripts/workspace-status.cmd"), 1
        )

    def test_explicit_workspace_producer_choices_are_preserved(self):
        for choice in (
            ("--workspace_status_command=custom-producer.cmd",),
            ("--workspace_status_command", "custom producer.cmd"),
        ):
            with self.subTest(choice=choice):
                _, args = self.successful_call(
                    flags=("--windows-cross-compile",), args=choice
                )
                self.assertNotIn(
                    "--workspace_status_command=scripts/workspace-status.cmd", args
                )
                for arg in choice:
                    self.assertIn(arg, args)

    def test_empty_or_missing_workspace_producer_fails_before_bazel(self):
        for choice in (
            ("--workspace_status_command=",),
            ("--workspace_status_command",),
            ("--workspace_status_command", ""),
            ("--workspace_status_command", "--keep_going"),
        ):
            with self.subTest(choice=choice):
                result, calls = self.invoke(
                    flags=("--windows-cross-compile",), args=choice
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
                self.assertIn(
                    "requires a nonempty --workspace_status_command", result.stderr
                )

    def test_workspace_producer_binding_is_scoped_to_keyless_fork_actions(self):
        for environment in (
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"GITHUB_REPOSITORY": "other/fork"},
            {"GITHUB_ACTIONS": "false"},
            {"BUILDBUDDY_API_KEY": "synthetic-command-test-key"},
            {"RUNNER_OS": "Linux"},
        ):
            with self.subTest(environment=environment):
                _, args = self.successful_call(
                    flags=("--windows-cross-compile",), env=environment
                )
                self.assertFalse(
                    any(arg.startswith("--workspace_status_command") for arg in args)
                )

    def test_payload_workspace_status_literal_does_not_suppress_binding(self):
        payload = (TARGET, "--workspace_status_command=payload-literal")
        _, args = self.successful_call(
            flags=("--windows-cross-compile",), targets=payload
        )
        separator = args.index("--")
        self.assertIn(
            "--workspace_status_command=scripts/workspace-status.cmd", args[:separator]
        )
        self.assertEqual(args[separator + 1 :], list(payload))

    def invoke(
        self,
        *,
        args=(),
        flags=(),
        env=None,
        targets=(TARGET,),
        provision=True,
        command="test",
        startup=(),
        metadata=False,
        real_wrapper=False,
    ):
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
            stub.write_text(
                Path(__file__)
                .with_name("run_bazel_with_buildbuddy.py")
                .read_text(encoding="utf-8")
                if real_wrapper
                else STUB,
                encoding="utf-8",
                newline="\n",
            )
            stub.chmod(0o755)
            record = root / "calls.jsonl"
            tools = root / "voice tools"
            if provision:
                (tools / "cygwin").mkdir(parents=True)
                (tools / "pkgconf-image").mkdir()
                (tools / "voice-tools.json").write_text("{}", encoding="utf-8")
            system_root = os.environ.get("SystemRoot", r"C:\Windows")
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
                SystemRoot=system_root,
                PROCESSOR_ARCHITECTURE="AMD64",
                VOICE_WINDOWS_SYSTEM_ROOT=system_root,
                VOICE_WINDOWS_HOST_ARCH="AMD64",
                VOICE_WINDOWS_BAZEL_REPOSITORY=str(tools),
            )
            if metadata:
                metadata_root = root / "voice-metadata-bazel-fixture"
                metadata_root.mkdir()
                environment.update(
                    {
                        name: "fixture-sdk"
                        for name in (
                            "INCLUDE",
                            "LIB",
                            "LIBPATH",
                            "UCRTVersion",
                            "UniversalCRTSdkDir",
                            "VCINSTALLDIR",
                            "VCToolsInstallDir",
                            "WindowsLibPath",
                            "WindowsSdkBinPath",
                            "WindowsSdkDir",
                            "WindowsSDKLibVersion",
                            "WindowsSDKVersion",
                        )
                    }
                )
                environment.update(
                    VOICE_CONFIGURED_METADATA_MODE="bounded-v1",
                    GITHUB_EVENT_NAME="workflow_dispatch",
                    GITHUB_JOB="voice-runtime-diagnostic",
                    RUNNER_TEMP=str(root),
                    BAZEL_OUTPUT_USER_ROOT=str(metadata_root),
                )
            if real_wrapper:
                # ROOT runs this native Git Bash probe after source review. The
                # fixture exports only equality/binding booleans, never PATH/argv.
                fake_python = root / "fake_bazel.py"
                fake_python.write_text(
                    "import json,os,sys\nfrom pathlib import Path\n"
                    "args=sys.argv[1:]\n"
                    "Path(os.environ['BAZEL_STUB_RECORD']).write_text(json.dumps({"
                    "'path_matches_selected':os.environ.get('PATH')==os.environ['CODEX_BAZEL_WINDOWS_PATH'],"
                    "'named_bindings':all(args.count(x+'=PATH')==1 for x in ('--action_env','--host_action_env','--test_env')),"
                    "'no_path_values':not any(x.startswith(('--action_env=PATH=','--host_action_env=PATH=','--test_env=PATH=')) for x in args),"
                    "'batch_query':'--batch' in args and 'cquery' in args})+'\\n')\n",
                    encoding="utf-8",
                )
                fake_cmd = root / "fake_bazel.cmd"
                fake_cmd.write_text(
                    '@"' + sys.executable + '" "%~dp0fake_bazel.py" %*\n',
                    encoding="utf-8",
                )
                environment["CODEX_BAZEL_BIN"] = str(fake_cmd)
            for name, value in (env or {}).items():
                if value is None:
                    environment.pop(name, None)
                else:
                    environment[name] = value
            result = subprocess.run(
                [
                    bash,
                    str(script),
                    *flags,
                    "--",
                    *startup,
                    command,
                    *args,
                    "--",
                    *targets,
                ],
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

    def test_owned_metadata_queries_use_three_named_bindings_and_keep_sdk_flags(self):
        for command in ("cquery", "aquery"):
            result, args = self.successful_call(
                metadata=True,
                command=command,
                startup=("--batch",),
                flags=("--windows-cross-compile",),
            )
            for option in ("--action_env", "--host_action_env", "--test_env"):
                self.assertEqual(args.count(option + "=PATH"), 1)
                self.assertFalse(any(arg.startswith(option + "=PATH=") for arg in args))
            self.assertIn("--action_env=UCRTVersion", args)
            self.assertIn("--host_action_env=WindowsSdkDir", args)
            self.assertIn("--config=ci-windows-msvc", args)
            self.assertIn(
                "--workspace_status_command=scripts/workspace-status.cmd", args
            )
            self.assertIn("--batch", args)
            self.assertIn("--noexperimental_remote_repo_contents_cache", args)
            self.assertIn(DECLARATION, args)
            self.assertIn("GNU cross compilation is not exercised", result.stdout)

    def test_metadata_mode_rejects_wrong_context_missing_sdk_and_path_overrides(self):
        for environment in (
            {"VOICE_CONFIGURED_METADATA_MODE": "wrong"},
            {"GITHUB_REPOSITORY": "other/fork"},
            {"GITHUB_EVENT_NAME": "push"},
            {"GITHUB_JOB": "ordinary"},
            {"RUNNER_OS": "Linux"},
            {"BUILDBUDDY_API_KEY": "fixture-key"},
            {"INCLUDE": ""},
            {"CODEX_BAZEL_WINDOWS_PATH": ""},
            {"BAZEL_OUTPUT_USER_ROOT": "C:/not-private"},
            {"VOICE_CONFIGURED_METADATA_MODE": None},
        ):
            result, calls = self.invoke(
                metadata=True,
                command="cquery",
                startup=("--batch",),
                flags=("--windows-cross-compile",),
                env=environment,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])
        for option in ("--action_env", "--host_action_env", "--test_env"):
            for choice in (
                (option + "=PATH=fixture-private",),
                (option, "Path=fixture-private"),
                (option + "==PATH",),
            ):
                result, calls = self.invoke(
                    metadata=True,
                    command="cquery",
                    startup=("--batch",),
                    flags=("--windows-cross-compile",),
                    args=choice,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
        for startup, command in (
            ((), "cquery"),
            (("--batch", "--batch"), "cquery"),
            (("--batch",), "test"),
        ):
            result, calls = self.invoke(
                metadata=True,
                command=command,
                startup=startup,
                flags=("--windows-cross-compile",),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])

    def test_no_mode_runtime_or_other_job_retains_explicit_path_values(self):
        for command, environment in (
            ("test", {"VOICE_CONFIGURED_METADATA_MODE": None}),
            (
                "cquery",
                {"VOICE_CONFIGURED_METADATA_MODE": None, "GITHUB_JOB": "ordinary"},
            ),
        ):
            _, args = self.successful_call(
                metadata=True,
                command=command,
                startup=("--batch",),
                flags=("--windows-cross-compile",),
                env=environment,
            )
            for option in ("--action_env", "--host_action_env", "--test_env"):
                self.assertIn(
                    option + r"=PATH=C:\Program Files\SDK;C:\Windows\System32", args
                )
                self.assertNotIn(option + "=PATH", args)

    @unittest.skipUnless(os.name == "nt", "ROOT native Windows Git Bash probe")
    def test_real_git_bash_python_wrapper_fake_bazel_receives_exact_selected_path(self):
        result, calls = self.invoke(
            metadata=True,
            real_wrapper=True,
            command="cquery",
            startup=("--batch",),
            flags=("--windows-cross-compile",),
        )
        self.assertEqual(result.returncode, 0, "Native fake-Bazel probe failed")
        self.assertEqual(
            calls,
            [
                {
                    "path_matches_selected": True,
                    "named_bindings": True,
                    "no_path_values": True,
                    "batch_query": True,
                }
            ],
        )

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
            provision=False,
        )
        self.assertIn("--config=ci-windows-cross", args)
        self.assertIn("--host_platform=//:rbe", args)
        self.assertIn("--shell_executable=/bin/bash", args)
        self.assertNotIn("--config=ci-windows-msvc", args)
        self.assertNotIn("--platforms=//:windows_x86_64_msvc", args)
        self.assertIn("--action_env=PATH=/usr/bin:/bin", args)
        self.assertIn(DECLARATION, args)
        self.assertFalse(any("voice_windows_tools=" in arg for arg in args))
        self.assertFalse(any("--action_env=SystemRoot=" in arg for arg in args))

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

    def test_native_fallback_passes_fixed_actual_environment_and_declared_tools(self):
        _, args = self.successful_call(flags=("--windows-cross-compile",))
        system_root = os.environ.get("SystemRoot", r"C:\Windows")
        for prefix in ("--action_env", "--host_action_env"):
            self.assertIn(f"{prefix}=SystemRoot={system_root}", args)
            self.assertIn(f"{prefix}=PROCESSOR_ARCHITECTURE=AMD64", args)
            self.assertNotIn(f"{prefix}=SystemRoot", args)
        self.assertIn(
            "--//third_party/voice:windows_installed_tools=@voice_windows_tools//:tools",
            args,
        )
        self.assertIn(
            "--extra_toolchains=//third_party/voice:windows_pkg_config_toolchain,//third_party/voice:windows_cmake_toolchain",
            args,
        )
        repositories = [arg for arg in args if arg.startswith("--inject_repository=")]
        self.assertEqual(len(repositories), 1)
        self.assertIn("voice tools", repositories[0])

    def test_missing_native_prerequisites_fail_before_child_contact(self):
        for name in (
            "VOICE_WINDOWS_SYSTEM_ROOT",
            "VOICE_WINDOWS_HOST_ARCH",
            "VOICE_WINDOWS_BAZEL_REPOSITORY",
        ):
            with self.subTest(name=name):
                result, calls = self.invoke(
                    flags=("--windows-cross-compile",), env={name: ""}
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
                self.assertIn(name, result.stderr)

    def test_native_values_must_match_actual_process_host(self):
        for env in (
            {"VOICE_WINDOWS_SYSTEM_ROOT": "not-the-real-SystemRoot"},
            {"VOICE_WINDOWS_HOST_ARCH": "ARM64"},
            {"PROCESSOR_ARCHITECTURE": "ARM64"},
            {"PROCESSOR_ARCHITEW6432": "ARM64"},
        ):
            with self.subTest(env=env):
                result, calls = self.invoke(flags=("--windows-cross-compile",), env=env)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
                self.assertIn("actual SystemRoot and AMD64", result.stderr)

    def test_missing_installed_tree_fails_before_child_contact(self):
        result, calls = self.invoke(flags=("--windows-cross-compile",), provision=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("complete provisioned Cygwin/pkgconf repository", result.stderr)

    def test_explicit_native_input_overrides_are_not_silently_replaced(self):
        for option, value in (
            ("--action_env", "SystemRoot=other"),
            ("--action_env", "SYSTEMROOT=other"),
            ("--host_action_env", "systemroot=other"),
            ("--host_action_env", "PROCESSOR_ARCHITECTURE=other"),
            ("--inject_repository", "voice_windows_tools=other"),
            ("--//third_party/voice:windows_installed_tools", "@other//:tools"),
        ):
            for separate in (False, True):
                with self.subTest(option=option, separate=separate):
                    args = (option, value) if separate else (f"{option}={value}",)
                    result, calls = self.invoke(
                        flags=("--windows-cross-compile",), args=args
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(calls, [])
                    self.assertIn("verified build inputs", result.stderr)

    def test_unrelated_environment_settings_remain_caller_owned(self):
        original = (
            "--action_env=SystemRoot_NOTE=retained",
            "--host_action_env=OTHER=ok",
        )
        _, args = self.successful_call(
            flags=("--windows-cross-compile",), args=original
        )
        self.assertEqual(args[2 : 2 + len(original)], list(original))

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
