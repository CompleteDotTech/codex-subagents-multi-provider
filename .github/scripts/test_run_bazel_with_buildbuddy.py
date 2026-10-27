#!/usr/bin/env python3

import json
import contextlib
import hashlib
import ctypes as c
import os
import subprocess
import sys
import time
import unittest
from unittest import mock
from pathlib import Path
from tempfile import TemporaryDirectory

import run_bazel_with_buildbuddy


class RunBazelWithBuildBuddyTest(unittest.TestCase):
    def github_env(
        self,
        temp_dir: str,
        *,
        repository: str = "openai/codex",
        fork: bool = False,
        event_name: str = "pull_request",
    ) -> dict[str, str]:
        event_path = Path(temp_dir) / "event.json"
        event_path.write_text(
            json.dumps({"pull_request": {"head": {"repo": {"fork": fork}}}}),
            encoding="utf-8",
        )
        return {
            "BUILDBUDDY_API_KEY": "token",
            "GITHUB_ACTIONS": "true",
            "GITHUB_EVENT_NAME": event_name,
            "GITHUB_EVENT_PATH": str(event_path),
            "GITHUB_REPOSITORY": repository,
        }

    def test_keyless_invocation_drops_remote_ci_configuration(self) -> None:
        self.assertIsNone(
            run_bazel_with_buildbuddy.remote_config(
                ["build", "--config=ci-linux", "//codex-rs/cli:codex"],
                {},
            )
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                ["build", "--config=ci-linux", "--", "//codex-rs/cli:codex"],
                {},
            ),
            ["build", "--", "//codex-rs/cli:codex"],
        )

    def test_program_arguments_after_separator_do_not_select_or_lose_rbe(self) -> None:
        args = ["run", "//codex-rs/cli:codex", "--", "--config=remote"]

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(args, {}),
            args,
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.remote_config(
                args, {"BUILDBUDDY_API_KEY": "fork-token"}
            ),
            "buildbuddy-generic",
        )

    def test_upstream_push_selects_openai_rbe_before_target_separator(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, event_name="push")

            self.assertEqual(
                run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                    ["build", "--config=ci-linux", "--", "//codex-rs/cli:codex"],
                    env,
                ),
                [
                    "build",
                    "--config=buildbuddy-openai-rbe",
                    "--remote_header=x-buildbuddy-api-key=token",
                    "--config=ci-linux",
                    "--",
                    "//codex-rs/cli:codex",
                ],
            )

    def test_windows_cross_ci_configuration_follows_remote_configuration(self) -> None:
        env = {"BUILDBUDDY_API_KEY": "fork-token"}

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                ["build", "--config=ci-windows-cross", "//codex-rs/cli:codex"],
                env,
            ),
            [
                "build",
                "--config=buildbuddy-generic-rbe",
                "--remote_header=x-buildbuddy-api-key=fork-token",
                "--config=ci-windows-cross",
                "//codex-rs/cli:codex",
            ],
        )

    def test_query_remote_configuration_is_inserted_before_expression(self) -> None:
        expression = 'kind("rust_library rule", //codex-rs/...)'
        env = {"BUILDBUDDY_API_KEY": "fork-token"}

        for command in ("query", "cquery", "aquery"):
            with self.subTest(command=command):
                self.assertEqual(
                    run_bazel_with_buildbuddy.bazel_args_with_remote_config(
                        [
                            command,
                            "--config=ci-windows-cross",
                            "--output=label",
                            expression,
                        ],
                        env,
                    ),
                    [
                        command,
                        "--config=buildbuddy-generic-rbe",
                        "--remote_header=x-buildbuddy-api-key=fork-token",
                        "--config=ci-windows-cross",
                        "--output=label",
                        expression,
                    ],
                )

    def test_same_repository_pull_request_selects_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], self.github_env(temp_dir)
                ),
                "buildbuddy-openai-rbe",
            )

    def test_fork_pull_request_cannot_select_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, fork=True)

            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], env
                ),
                "buildbuddy-generic-rbe",
            )

    def test_run_in_fork_repository_cannot_select_openai_host(self) -> None:
        with TemporaryDirectory() as temp_dir:
            env = self.github_env(temp_dir, repository="contributor/codex")

            self.assertEqual(
                run_bazel_with_buildbuddy.remote_config(
                    ["build", "--config=ci-v8"], env
                ),
                "buildbuddy-generic-rbe",
            )

    def test_pull_request_without_readable_event_payload_fails_closed(self) -> None:
        for event_path in (None, "missing-event.json"):
            env = {
                "BUILDBUDDY_API_KEY": "token",
                "GITHUB_ACTIONS": "true",
                "GITHUB_EVENT_NAME": "pull_request",
                "GITHUB_REPOSITORY": "openai/codex",
            }
            if event_path is not None:
                env["GITHUB_EVENT_PATH"] = event_path

            with self.subTest(event_path=event_path):
                self.assertEqual(
                    run_bazel_with_buildbuddy.remote_config(["build"], env),
                    "buildbuddy-generic",
                )

    def test_bazel_command_uses_configured_binary_locally(self) -> None:
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "info",
                "execution_root",
                env={"CODEX_BAZEL_BIN": "fake-bazel"},
            ),
            ["fake-bazel", "info", "execution_root"],
        )

    def test_bazel_command_normalizes_github_actions_startup_options(self) -> None:
        env = {
            "BAZEL_OUTPUT_USER_ROOT": "/tmp/bazel-output",
            "GITHUB_ACTIONS": "true",
        }

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command("build", "//codex-rs/...", env=env),
            [
                "bazel",
                "--output_user_root=/tmp/bazel-output",
                "--noexperimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
            ],
        )
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "--experimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
                env=env,
            ),
            [
                "bazel",
                "--output_user_root=/tmp/bazel-output",
                "--experimental_remote_repo_contents_cache",
                "build",
                "//codex-rs/...",
            ],
        )

    def test_bazel_command_uses_configured_local_caches(self) -> None:
        env = {
            "BAZEL_REPO_CONTENTS_CACHE": "/tmp/bazel-repo-contents",
            "BAZEL_REPOSITORY_CACHE": "/tmp/bazel-repository",
        }

        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "build",
                "--config=local",
                "//codex-rs/...",
                env=env,
            ),
            [
                "bazel",
                "build",
                "--config=local",
                "//codex-rs/...",
                "--repo_contents_cache=/tmp/bazel-repo-contents",
                "--repository_cache=/tmp/bazel-repository",
            ],
        )

    def test_bazel_command_adds_local_caches_before_separator(self) -> None:
        self.assertEqual(
            run_bazel_with_buildbuddy.bazel_command(
                "build",
                "//codex-rs/...",
                "--",
                "--program-arg",
                env={"BAZEL_REPOSITORY_CACHE": "/tmp/bazel-repository"},
            ),
            [
                "bazel",
                "build",
                "//codex-rs/...",
                "--repository_cache=/tmp/bazel-repository",
                "--",
                "--program-arg",
            ],
        )

    def test_main_preserves_spaced_argument_and_child_exit_status(self) -> None:
        spaced_arg = (
            r"--test_env=PATH=C:\Program Files\PowerShell\7;C:\Program Files\Git\bin"
        )
        child_code = (
            f"import sys; sys.exit(37 if sys.argv[1] == {spaced_arg!r} else 91)"
        )
        env = os.environ.copy()
        env["CODEX_BAZEL_BIN"] = sys.executable
        env.pop("BAZEL_OUTPUT_USER_ROOT", None)
        env.pop("BUILDBUDDY_API_KEY", None)
        env.pop("GITHUB_ACTIONS", None)

        result = subprocess.run(
            [
                sys.executable,
                str(Path(run_bazel_with_buildbuddy.__file__)),
                "-c",
                child_code,
                spaced_arg,
            ],
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 37, result.stderr)


class MetadataPathModeTest(unittest.TestCase):
    def invocation(self, directory):
        temp = Path(directory)
        root = temp / "voice-metadata-bazel-fixture"
        root.mkdir(exist_ok=True)
        private = temp / "evidence" / "private"
        private.mkdir(parents=True, exist_ok=True)
        env = {name: "fixture" for name in run_bazel_with_buildbuddy.METADATA_SDK_NAMES}
        env.update(
            {
                "VOICE_CONFIGURED_METADATA_MODE": "bounded-v1",
                "RUNNER_OS": "Windows",
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": "CompleteDotTech/codex-subagents-multi-provider",
                "GITHUB_EVENT_NAME": "workflow_dispatch",
                "GITHUB_JOB": "voice-runtime-diagnostic",
                "RUNNER_TEMP": directory,
                "BAZEL_OUTPUT_USER_ROOT": str(root),
                "CODEX_BAZEL_WINDOWS_PATH": "C:/PRIVATE tool/\U0001f642;C:/Windows",
                "SYSTEMROOT": "C:/Windows",
                "VOICE_WINDOWS_SYSTEM_ROOT": "C:/Windows",
                "PROCESSOR_ARCHITECTURE": "AMD64",
                "VOICE_WINDOWS_HOST_ARCH": "AMD64",
                "VOICE_WINDOWS_BAZEL_REPOSITORY": "fixture-tools",
                "PATH": "original-tool-search-path",
                "VOICE_METADATA_PRIVATE_DIR": str(private),
                "VOICE_METADATA_STATUS_PATH": str(private / "launcher-provider-0.json"),
                "VOICE_METADATA_STATUS_NONCE": "a" * 32,
                "VOICE_METADATA_STATUS_PHASE": "provider-0",
                "VOICE_METADATA_DEADLINE": str(time.monotonic() + 120),
            }
        )
        args = [
            "--batch",
            "cquery",
            "--action_env=PATH",
            "--host_action_env=PATH",
            "--test_env=PATH",
            "--",
            "//fixture:target",
        ]
        return args, env

    def test_actual_main_spawn_uses_selected_path_and_original_executable_search(self):
        with TemporaryDirectory() as directory:
            args, env = self.invocation(directory)
            with (
                mock.patch.dict(os.environ, env, clear=True),
                mock.patch.object(sys, "argv", ["wrapper", *args]),
                mock.patch.object(
                    run_bazel_with_buildbuddy.shutil,
                    "which",
                    return_value=sys.executable,
                ) as which,
                mock.patch.object(
                    run_bazel_with_buildbuddy.subprocess,
                    "run",
                    return_value=mock.Mock(returncode=37),
                ) as launched,
                mock.patch.object(
                    run_bazel_with_buildbuddy,
                    "metadata_launcher_lock",
                    return_value=contextlib.nullcontext(),
                ),
                mock.patch("builtins.print"),
            ):
                original = dict(os.environ)
                with self.assertRaises(SystemExit) as error:
                    run_bazel_with_buildbuddy.main()
                self.assertEqual(error.exception.code, 37)
                self.assertEqual(dict(os.environ), original)
            which.assert_called_once_with("bazel", path=env["PATH"])
            command = launched.call_args.args[0]
            child = launched.call_args.kwargs["env"]
            self.assertEqual(command[0], str(Path(sys.executable).resolve(strict=True)))
            self.assertEqual(
                child["PATH"].encode("utf-16-le"),
                env["CODEX_BAZEL_WINDOWS_PATH"].encode("utf-16-le"),
            )
            self.assertEqual(
                {k: v for k, v in child.items() if k != "PATH"},
                {
                    k: v
                    for k, v in env.items()
                    if k != "PATH"
                    and k not in run_bazel_with_buildbuddy.METADATA_STATUS_ENV
                },
            )
            for name in run_bazel_with_buildbuddy.METADATA_STATUS_ENV:
                self.assertNotIn(name, child)
            status = json.loads(Path(env["VOICE_METADATA_STATUS_PATH"]).read_text())
            self.assertIs(status["approved_launcher_sha256_match"], True)
            self.assertEqual(status["nonce"], "a" * 32)
            for option in run_bazel_with_buildbuddy.METADATA_PATH_OPTIONS:
                self.assertEqual(command.count(option + "=PATH"), 1)
                self.assertFalse(
                    any(arg.startswith(option + "=PATH=") for arg in command)
                )
            self.assertIn("--batch", command)
            self.assertIn(
                "--output_user_root=" + env["BAZEL_OUTPUT_USER_ROOT"], command
            )

    def test_invalid_contexts_sdk_mode_and_roots_reject_before_spawn(self):
        with TemporaryDirectory() as directory:
            args, env = self.invocation(directory)
            changes = [
                {name: "invalid"}
                for name in (
                    "VOICE_CONFIGURED_METADATA_MODE",
                    "RUNNER_OS",
                    "GITHUB_ACTIONS",
                    "GITHUB_REPOSITORY",
                    "GITHUB_EVENT_NAME",
                    "GITHUB_JOB",
                    "VOICE_WINDOWS_HOST_ARCH",
                    "VOICE_WINDOWS_SYSTEM_ROOT",
                    "BAZEL_OUTPUT_USER_ROOT",
                    "RUNNER_TEMP",
                )
            ]
            changes += [{"BUILDBUDDY_API_KEY": "PRIVATE-key"}]
            changes += [
                {name: ""}
                for name in (
                    *run_bazel_with_buildbuddy.METADATA_SDK_NAMES,
                    "CODEX_BAZEL_WINDOWS_PATH",
                )
            ]
            for change in changes:
                invalid = {**env, **change}
                with (
                    self.subTest(change=change),
                    mock.patch.object(
                        run_bazel_with_buildbuddy.subprocess, "run"
                    ) as launched,
                ):
                    with self.assertRaises(ValueError):
                        run_bazel_with_buildbuddy.metadata_path_mode(args, invalid)
                    launched.assert_not_called()
            invalid_args = [
                [arg for arg in args if arg != "--batch"],
                ["--batch", *args],
                ["--nobatch", *args],
                [arg.replace("cquery", "test") for arg in args],
                [
                    "--batch",
                    "cquery",
                    "--",
                    "--action_env=PATH",
                    "--host_action_env=PATH",
                    "--test_env=PATH",
                ],
            ]
            for option in run_bazel_with_buildbuddy.METADATA_PATH_OPTIONS:
                for value in ("PATH=PRIVATE", "Path=PRIVATE", "=PATH"):
                    invalid_args.append(
                        [
                            arg if arg != option + "=PATH" else option + "=" + value
                            for arg in args
                        ]
                    )
                    invalid_args.append(
                        [arg for arg in args if arg != option + "=PATH"][:-2]
                        + [option, value, "--", "//fixture:target"]
                    )
                invalid_args.append([*args[:2], option + "=PATH", *args[2:]])
            for invalid in invalid_args:
                with self.subTest(args=invalid), self.assertRaises(ValueError):
                    run_bazel_with_buildbuddy.metadata_path_mode(invalid, env)

    def test_actual_main_invalid_metadata_rejects_before_executable_or_child(self):
        with TemporaryDirectory() as directory:
            args, env = self.invocation(directory)
            invalid_calls = [
                (args, {**env, "VOICE_CONFIGURED_METADATA_MODE": "PRIVATE-invalid"}),
                (args, {**env, "GITHUB_JOB": "other-job"}),
                (args, {**env, "INCLUDE": ""}),
                (args, {**env, "BAZEL_OUTPUT_USER_ROOT": directory}),
                (
                    [
                        arg.replace(
                            "--action_env=PATH", "--action_env=PATH=PRIVATE-value"
                        )
                        for arg in args
                    ],
                    env,
                ),
            ]
            for invalid_args, invalid_env in invalid_calls:
                with (
                    mock.patch.dict(os.environ, invalid_env, clear=True),
                    mock.patch.object(sys, "argv", ["wrapper", *invalid_args]),
                    mock.patch.object(
                        run_bazel_with_buildbuddy.subprocess, "run"
                    ) as launched,
                    mock.patch.object(
                        run_bazel_with_buildbuddy.shutil, "which"
                    ) as which,
                    mock.patch("builtins.print") as printed,
                ):
                    before = dict(os.environ)
                    with self.assertRaises(SystemExit) as error:
                        run_bazel_with_buildbuddy.main()
                    self.assertEqual(error.exception.code, 1)
                    self.assertEqual(dict(os.environ), before)
                    self.assertNotIn("PRIVATE", str(printed.call_args_list))
                launched.assert_not_called()
                which.assert_not_called()

    def test_missing_mode_is_rejected_only_for_exact_metadata_private_route(self):
        with TemporaryDirectory() as directory:
            args, env = self.invocation(directory)
            del env["VOICE_CONFIGURED_METADATA_MODE"]
            with self.assertRaisesRegex(ValueError, "Metadata PATH mode is required"):
                run_bazel_with_buildbuddy.metadata_path_mode(args, env)
            for changed in (
                {"GITHUB_JOB": "ordinary"},
                {"GITHUB_EVENT_NAME": "push"},
                {"BAZEL_OUTPUT_USER_ROOT": directory},
            ):
                self.assertFalse(
                    run_bazel_with_buildbuddy.metadata_path_mode(
                        args, {**env, **changed}
                    )
                )
            self.assertFalse(
                run_bazel_with_buildbuddy.metadata_path_mode(
                    ["test", "//fixture:target"], env
                )
            )

    def test_default_main_preserves_original_spawn_contract(self):
        env = {"PATH": "original", "CODEX_BAZEL_WINDOWS_PATH": "selected"}
        for command in ("test", "cquery", "aquery"):
            with (
                mock.patch.dict(os.environ, env, clear=True),
                mock.patch.object(
                    sys, "argv", ["wrapper", command, "//fixture:target"]
                ),
                mock.patch.object(
                    run_bazel_with_buildbuddy.subprocess,
                    "run",
                    return_value=mock.Mock(returncode=0),
                ) as launched,
                mock.patch.object(run_bazel_with_buildbuddy.shutil, "which") as which,
                mock.patch("builtins.print"),
            ):
                with self.assertRaises(SystemExit):
                    run_bazel_with_buildbuddy.main()
                self.assertEqual(dict(os.environ), env)
            self.assertEqual(launched.call_args.kwargs, {"check": False})
            self.assertEqual(
                launched.call_args.args[0], ["bazel", command, "//fixture:target"]
            )
            which.assert_not_called()


class FakeLauncherAPI:
    def __init__(self, path, payload):
        self.path, self.payload, self.offset = str(path), payload, 0
        self.closed, self.changed, self.size_override = False, False, None
        self.calls = []

    def CreateFileW(self, *args):
        self.calls.append(("open", args))
        return 1234

    def GetFileInformationByHandle(self, handle, pointer):
        info = c.cast(
            pointer, c.POINTER(run_bazel_with_buildbuddy.LauncherInfo)
        ).contents
        info.attributes, info.volume, info.index_lo = 0x80, 7, 10 + self.changed
        info.size_lo = (
            len(self.payload) if self.size_override is None else self.size_override
        )
        return True

    def GetFinalPathNameByHandleW(self, handle, output, capacity, flags):
        output.value = self.path
        return len(self.path)

    def ReadFile(self, handle, output, capacity, count, overlapped):
        chunk = self.payload[self.offset : self.offset + capacity]
        c.memmove(output, chunk, len(chunk))
        c.cast(count, c.POINTER(run_bazel_with_buildbuddy.w.DWORD))[0] = len(chunk)
        self.offset += len(chunk)
        return True

    def CloseHandle(self, handle):
        self.closed = True
        return True


class MetadataAttestationTest(unittest.TestCase):
    def fixture(self, directory):
        root = Path(directory)
        for leaf in ("toolcache", "runner-temp", "workspace"):
            (root / leaf).mkdir()
        path = root / "toolcache/bazel.exe"
        path.write_bytes(b"APPROVED-SYNTHETIC-LAUNCHER")
        env = {
            "RUNNER_TOOL_CACHE": str(path.parent),
            "RUNNER_TEMP": str(root / "runner-temp"),
            "GITHUB_WORKSPACE": str(root / "workspace"),
        }
        return path, env, FakeLauncherAPI(path, path.read_bytes())

    def test_exact_file_lock_hash_and_lifetime(self):
        with TemporaryDirectory() as directory:
            path, env, api = self.fixture(directory)
            with mock.patch.object(
                run_bazel_with_buildbuddy,
                "METADATA_APPROVED_LAUNCHER_SHA256",
                hashlib.sha256(api.payload).hexdigest(),
            ):
                with run_bazel_with_buildbuddy.metadata_launcher_lock(
                    str(path), env, time.monotonic() + 10, api=api
                ):
                    self.assertFalse(api.closed)
                    self.assertEqual(api.offset, len(api.payload))
                self.assertTrue(api.closed)
            args = api.calls[0][1]
            self.assertEqual(args[1:5], (0x80000000, 1, None, 3))

    def test_hash_size_deadline_and_identity_change_fail_closed(self):
        for failure in ("hash", "size", "identity", "deadline"):
            with self.subTest(failure=failure), TemporaryDirectory() as directory:
                path, env, api = self.fixture(directory)
                if failure == "size":
                    api.size_override = 16 * 1024 * 1024 + 1
                expected = (
                    "0" * 64
                    if failure == "hash"
                    else hashlib.sha256(api.payload).hexdigest()
                )
                with mock.patch.object(
                    run_bazel_with_buildbuddy,
                    "METADATA_APPROVED_LAUNCHER_SHA256",
                    expected,
                ):
                    with self.assertRaises(ValueError):
                        with run_bazel_with_buildbuddy.metadata_launcher_lock(
                            str(path),
                            env,
                            time.monotonic() + (-1 if failure == "deadline" else 10),
                            api=api,
                        ):
                            self.assertEqual(failure, "identity")
                            api.changed = True
                self.assertTrue(api.closed or failure == "deadline")

    def test_untrusted_workspace_or_temp_selection_never_opens_file(self):
        with TemporaryDirectory() as directory:
            path, env, api = self.fixture(directory)
            env["GITHUB_WORKSPACE"] = str(path.parent)
            with self.assertRaises(ValueError):
                with run_bazel_with_buildbuddy.metadata_launcher_lock(
                    str(path), env, time.monotonic() + 10, api=api
                ):
                    self.fail("workspace launcher cannot be admitted")
            self.assertEqual(api.calls, [])

    def test_status_finalizes_only_after_locked_launch_returns(self):
        with TemporaryDirectory() as directory:
            args, env = MetadataPathModeTest().invocation(directory)
            command = [sys.executable, *args]
            child = {
                key: value
                for key, value in env.items()
                if key not in run_bazel_with_buildbuddy.METADATA_STATUS_ENV
            }
            path = Path(env["VOICE_METADATA_STATUS_PATH"])

            def launched(*args, **kwargs):
                self.assertEqual(path.read_bytes(), b"")
                self.assertFalse(
                    any(
                        name in kwargs["env"]
                        for name in run_bazel_with_buildbuddy.METADATA_STATUS_ENV
                    )
                )
                return mock.Mock(returncode=9)

            with (
                mock.patch.dict(os.environ, env, clear=True),
                mock.patch.object(
                    run_bazel_with_buildbuddy,
                    "metadata_launcher_lock",
                    return_value=contextlib.nullcontext(),
                ),
                mock.patch.object(
                    run_bazel_with_buildbuddy.subprocess, "run", side_effect=launched
                ),
            ):
                result = run_bazel_with_buildbuddy.run_metadata_launcher(command, child)
            self.assertEqual(result.returncode, 9)
            status = json.loads(path.read_bytes())
            self.assertIs(status["completed"], True)
            self.assertLessEqual(path.stat().st_size, 512)
            with (
                mock.patch.dict(os.environ, env, clear=True),
                self.assertRaises(ValueError),
            ):
                run_bazel_with_buildbuddy.metadata_status_context(env)


if __name__ == "__main__":
    unittest.main()
