"""No-building tests for storage policy, bounded observations and shell wiring."""

import contextlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import hosted_storage as storage
import run_bazel_with_buildbuddy as backend

ENV = dict(
    GITHUB_ACTIONS="true",
    GITHUB_REPOSITORY=storage.FORK,
    RUNNER_ENVIRONMENT="github-hosted",
    RUNNER_OS="Linux",
)


class PolicyTests(unittest.TestCase):
    def test_default_is_scoped_and_keeps_remote_configs_out(self):
        for os_name in ("Linux", "macOS"):
            self.assertEqual(
                storage.defaults(["test"], dict(ENV, RUNNER_OS=os_name)),
                ["--disk_cache="],
            )
        for change in (
            {"GITHUB_ACTIONS": "false"},
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"RUNNER_ENVIRONMENT": "self-hosted"},
            {"RUNNER_OS": "Windows"},
            {"BUILDBUDDY_API_KEY": "synthetic"},
        ):
            self.assertEqual(storage.defaults(["test"], dict(ENV, **change)), [])
        self.assertEqual(storage.defaults(["clean"], ENV), [])

    def test_explicit_empty_split_and_repeated_settings_and_payload(self):
        for flags in (
            ["--disk_cache="],
            ["--disk_cache", ""],
            ["--disk_cache", "cache with spaces"],
            ["--disk_cache=first", "--disk_cache", "last"],
        ):
            self.assertEqual(storage.defaults(["test", *flags], ENV), [])
        args = ["test", "--", "//target", "--disk_cache=payload"]
        self.assertEqual(storage.defaults(args, ENV), ["--disk_cache="])
        self.assertEqual(storage.info_options(args), [])

    def test_explicit_remote_routes_are_not_given_local_defaults(self):
        for flags in (
            ["--remote_executor=grpcs://synthetic"],
            ["--remote_cache", "https://synthetic"],
            ["--config", "buildbuddy-generic-rbe"],
            ["--strategy=Rustc=remote"],
        ):
            self.assertEqual(storage.defaults(["test", *flags], ENV), [])

    def test_local_only_strategies_and_last_value_for_same_mnemonic(self):
        for flags in (
            ["--strategy=Rustc=local"],
            ["--strategy", "Rustc=local,sandboxed"],
            ["--strategy=Rustc=remote", "--strategy=Rustc=local"],
        ):
            args = ["test", *flags]
            self.assertEqual(
                storage.effective_arguments(args, ENV),
                ["test", "--disk_cache=", *flags],
            )
        for flags in (
            ["--strategy=Rustc=local", "--strategy=Rustc=remote,sandboxed"],
            ["--strategy=Rustc=local", "--strategy=Genrule=remote"],
        ):
            self.assertEqual(storage.defaults(["test", *flags], ENV), [])


class BackendInteractionTests(unittest.TestCase):
    def test_final_endpoint_values_control_local_default_without_rewriting_flags(self):
        cases = [
            (["--remote_cache=https://synthetic", "--remote_cache="], True),
            (["--remote_cache=", "--remote_cache", "https://synthetic"], False),
            (["--remote_executor", "grpcs://synthetic", "--remote_executor", ""], True),
            (["--remote_executor", "", "--remote_executor=grpcs://synthetic"], False),
            (
                [
                    "--remote_cache=https://synthetic",
                    "--remote_executor=grpcs://synthetic",
                    "--remote_cache",
                    "",
                    "--remote_executor=",
                ],
                True,
            ),
            (
                [
                    "--remote_cache=",
                    "--remote_executor=grpcs://synthetic",
                    "--remote_cache=",
                ],
                False,
            ),
            (
                [
                    "--remote_executor=",
                    "--remote_cache=https://synthetic",
                    "--remote_executor",
                    "",
                ],
                False,
            ),
        ]
        payload = [
            "//target",
            "--remote_cache=https://payload",
            "--remote_executor",
            "payload",
        ]
        for flags, local in cases:
            with self.subTest(flags=flags):
                effective = storage.effective_arguments(
                    ["test", *flags, "--", *payload], ENV
                )
                command = backend.bazel_command(*effective, env=ENV)
                before = command[command.index("test") + 1 : command.index("--")]
                self.assertEqual(before, (["--disk_cache="] if local else []) + flags)
                self.assertEqual(command[command.index("--") + 1 :], payload)

    def test_empty_endpoints_do_not_override_other_explicit_remote_or_cache_choices(
        self,
    ):
        endpoints = [
            "--remote_cache=https://synthetic",
            "--remote_cache=",
            "--remote_executor",
            "",
        ]
        for choice in (
            ["--disk_cache", "owned"],
            ["--disk_cache="],
            ["--config=buildbuddy-generic-rbe"],
            ["--strategy=Rustc=remote"],
        ):
            effective = storage.effective_arguments(
                ["test", *endpoints, *choice, "--", "//target"], ENV
            )
            command = backend.bazel_command(*effective, env=ENV)
            before = command[command.index("test") + 1 : command.index("--")]
            self.assertEqual(before, endpoints + choice)

    def test_keyless_configs_have_identical_local_build_and_info_routes(self):
        for name in ("ci-linux", "ci-macos", "ci-v8"):
            for flags in (["--config=" + name], ["--config", name]):
                with self.subTest(flags=flags):
                    payload = ["//target", "--config=" + name, "--config", name]
                    effective = storage.effective_arguments(
                        ["test", *flags, "--", *payload], ENV
                    )
                    build = backend.bazel_command(*effective, env=ENV)
                    info = backend.bazel_command(
                        "info",
                        *storage.info_options(effective),
                        "bazel-testlogs",
                        env=ENV,
                    )
                    self.assertIn("--disk_cache=", build)
                    self.assertIn("--disk_cache=", info)
                    self.assertNotIn("--config=" + name, build[: build.index("--")])
                    self.assertNotIn("--config=" + name, info)
                    self.assertNotIn(name, build[: build.index("--")])
                    self.assertEqual(build[build.index("--") + 1 :], payload)
                    self.assertEqual(
                        storage.effective_arguments(effective, ENV), effective
                    )

    def test_explicit_cache_remains_owned_after_real_backend_fallback(self):
        for cache in (
            ["--disk_cache=first", "--disk_cache", "last"],
            ["--disk_cache", ""],
            ["--disk_cache="],
        ):
            effective = storage.effective_arguments(
                ["test", "--config", "ci-linux", *cache, "--", "//target"], ENV
            )
            command = backend.bazel_command(*effective, env=ENV)
            self.assertEqual(
                command[command.index("test") + 1 : command.index("--")], cache
            )

    def test_new_normalization_does_not_change_other_routes(self):
        for change in (
            {"BUILDBUDDY_API_KEY": "synthetic-key"},
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"RUNNER_OS": "Windows"},
            {"GITHUB_ACTIONS": "false"},
        ):
            env = dict(ENV, **change)
            for flags in (["--config=ci-linux"], ["--config", "ci-linux"]):
                args = ["test", *flags, "--", "//target"]
                effective = storage.effective_arguments(args, env)
                self.assertEqual(effective, args)
                self.assertEqual(
                    backend.bazel_command(*effective, env=env),
                    backend.bazel_command(*args, env=env),
                )

    def test_real_remote_endpoints_are_retained_by_actual_backend(self):
        for flags in (
            ["--remote_executor=grpcs://synthetic"],
            ["--remote_cache", "https://synthetic"],
        ):
            effective = storage.effective_arguments(
                ["test", "--config=ci-linux", *flags, "--", "//target"], ENV
            )
            command = backend.bazel_command(*effective, env=ENV)
            self.assertNotIn("--disk_cache=", command)
            for flag in flags:
                self.assertIn(flag, command)


class ProjectionTests(unittest.TestCase):
    def test_info_projection_preserves_order_and_actual_configuration(self):
        args = [
            "test",
            "--disk_cache=",
            "--compilation_mode",
            "opt",
            "-c",
            "fastbuild",
            "--platforms=//:one",
            "--host_platform",
            "//:host",
            "--config",
            "caller",
            "--repository_cache",
            "a b",
            "--execution_log_compact_file=omit",
            "--test_env=SECRET=omit",
            "--",
            "--platforms=payload",
        ]
        self.assertEqual(
            storage.info_options(args),
            [
                "--disk_cache=",
                "--compilation_mode=opt",
                "--compilation_mode=fastbuild",
                "--platforms=//:one",
                "--host_platform=//:host",
                "--config=caller",
                "--repository_cache=a b",
            ],
        )


class ActualPathTests(unittest.TestCase):
    def test_query_client_nonzero_exit_is_failure_without_output_payload(self):
        child = Mock()
        child.poll.return_value = 9
        child.wait.return_value = 9
        selector = Mock()
        selector.select.return_value = [object()]
        manager = Mock()
        manager.__enter__ = Mock(return_value=selector)
        manager.__exit__ = Mock(return_value=False)
        with (
            patch.object(storage.subprocess, "Popen", return_value=child),
            patch.object(storage.selectors, "DefaultSelector", return_value=manager),
            patch.object(storage.os, "read", return_value=b""),
        ):
            self.assertEqual(
                storage.bounded_query(["fake-bazel"], {}), ("failed", None)
            )
        child.kill.assert_not_called()
        child.stdout.close.assert_called_once_with()

    def test_actual_backend_query_uses_build_startup_and_effective_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = {
                key: str(Path(directory) / key)
                for key in ("output_base", "output_path", "execution_root")
            }
            raw = "".join(
                key + ": " + value + "\n" for key, value in paths.items()
            ).encode()
            env = dict(ENV, BAZEL_OUTPUT_USER_ROOT=directory)
            with patch.object(
                storage, "bounded_query", return_value=("ok", raw)
            ) as query:
                state, found = storage.actual_paths(
                    [
                        "test",
                        "--config",
                        "ci-linux",
                        "-c",
                        "opt",
                        "--platforms",
                        "//:custom",
                        "--",
                        "--test_env=SECRET",
                    ],
                    env,
                )
            self.assertEqual((state, found), ("ok", paths))
            command, actual_env = query.call_args.args
            self.assertIs(actual_env, env)
            self.assertIn("--output_user_root=" + directory, command)
            self.assertIn("--noexperimental_remote_repo_contents_cache", command)
            self.assertIn("--compilation_mode=opt", command)
            self.assertIn("--platforms=//:custom", command)
            self.assertIn("--disk_cache=", command)
            self.assertNotIn("ci-linux", command)
            self.assertFalse(any("SECRET" in arg for arg in command))

    def test_query_failures_do_not_invent_actual_or_zero_metrics(self):
        for failure in ("timeout", "failed", "output-limit"):
            with patch.object(storage, "bounded_query", return_value=(failure, None)):
                self.assertEqual(storage.actual_paths(["test"], ENV), (failure, None))
        for raw in (
            b"SECRET ERROR",
            b"output_base: relative\n",
            b"output_base: /a\noutput_base: /b\n",
        ):
            with patch.object(storage, "bounded_query", return_value=("ok", raw)):
                state, found = storage.actual_paths(["test"], ENV)
                self.assertNotEqual(state, "ok")
                self.assertIsNone(found)

    def test_query_cannot_run_outside_hosted_keyless_scope(self):
        for change in (
            {"RUNNER_OS": "Windows"},
            {"BUILDBUDDY_API_KEY": "synthetic"},
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"RUNNER_ENVIRONMENT": "self-hosted"},
            {"HOSTED_STORAGE_REMOTE_CREDENTIALS": "true"},
        ):
            with patch.object(storage, "bounded_query") as query:
                self.assertEqual(
                    storage.actual_paths(["test"], dict(ENV, **change)),
                    ("out-of-scope", None),
                )
                query.assert_not_called()

    def test_query_timeout_and_output_limit_clean_up_only_owned_client(self):
        for ready, chunk, expected in (
            ([], b"", "timeout"),
            ([object()], b"x" * 17, "output-limit"),
        ):
            child = Mock()
            child.poll.return_value = None
            selector = Mock()
            selector.select.return_value = ready
            manager = Mock()
            manager.__enter__ = Mock(return_value=selector)
            manager.__exit__ = Mock(return_value=False)
            with (
                patch.object(storage.subprocess, "Popen", return_value=child) as spawn,
                patch.object(
                    storage.selectors, "DefaultSelector", return_value=manager
                ),
                patch.object(storage.os, "read", return_value=chunk),
                patch.object(storage.os, "killpg", create=True) as kill_group,
                patch.object(storage.signal, "SIGKILL", 9, create=True),
            ):
                self.assertEqual(
                    storage.bounded_query(["fake-bazel"], {}, limit=16),
                    (expected, None),
                )
            kill_group.assert_called_once_with(child.pid, 9)
            child.kill.assert_not_called()
            child.wait.assert_called_once_with(timeout=2)
            child.stdout.close.assert_called_once_with()
            self.assertEqual(spawn.call_args.kwargs["stderr"], subprocess.DEVNULL)
            self.assertTrue(spawn.call_args.kwargs["start_new_session"])

    def test_verified_receipt_has_only_three_actual_roots_with_overlap(self):
        with tempfile.TemporaryDirectory() as directory:
            verified = dict(
                output_base=directory,
                output_path=str(Path(directory) / "out"),
                execution_root=str(Path(directory) / "exec"),
            )
            with (
                patch.object(storage, "measure", return_value={"available_bytes": 42}),
                patch.object(storage.subprocess, "run", side_effect=OSError),
            ):
                result = storage.receipt(
                    "after-build-paths", ["test"], ENV, 37, verified, "ok"
                )
        self.assertEqual(set(result["roots"]), set(verified))
        self.assertEqual(result["paths_are"], "actual bounded bazel info")
        self.assertIn("output_base", result["roots"]["output_path"]["overlaps"])


class WorkflowTests(unittest.TestCase):
    def test_cache_save_and_upload_are_scoped_without_changing_save_gate(self):
        workflow = (Path(__file__).parents[1] / "workflows/bazel.yml").read_text()
        jobs = dict(
            re.findall(
                r"^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)",
                workflow.split("jobs:\n", 1)[1],
                re.M | re.S,
            )
        )
        for name, block in jobs.items():
            if name not in ("test", "clippy", "verify-release-build"):
                self.assertNotIn("hosted_storage", block)
                continue
            before = block.index("- name: Observe storage before cache archive")
            save = block.index("- name: Save bazel repository cache")
            after = block.index("- name: Observe storage after cache archive")
            upload = block.index("- name: Upload hosted storage observations")
            self.assertLess(before, save)
            self.assertLess(save, after)
            self.assertLess(after, upload)
            self.assertIn(
                "if: always() && !cancelled() && steps.prepare_bazel.outputs.repository-cache-hit != 'true'",
                block[save:after],
            )
            self.assertIn("continue-on-error: true", block[save:after])
            self.assertIn("runner.environment == 'github-hosted'", block[before:save])
            self.assertIn(
                "runner.os == 'Linux' || runner.os == 'macOS'", block[before:save]
            )
            self.assertIn(
                "github.repository == '" + storage.FORK + "'", block[before:save]
            )
            self.assertIn(
                "HOSTED_STORAGE_REMOTE_CREDENTIALS: ${{ secrets.BUILDBUDDY_API_KEY != '' }}",
                block[before:save],
            )
            self.assertIn(
                "steps.hosted_storage.outputs.enabled == 'true'", block[after:upload]
            )
            self.assertIn(
                "steps.prepare_bazel.outputs.repository-cache-hit != 'true'",
                block[after:upload],
            )
            self.assertIn(
                "steps.hosted_storage.outputs.enabled == 'true'", block[upload:]
            )
            self.assertIn(
                "actions/upload-artifact@bbbca2ddaa5d8feaa63e36b76fdaad77386f024f",
                block[upload:],
            )
            self.assertIn(
                "path: ${{ runner.temp }}/hosted-storage/*.json", block[upload:]
            )


class RestoreCredentialTests(unittest.TestCase):
    def test_every_prepare_caller_explicitly_reports_credential_availability(self):
        github = Path(__file__).parents[1]
        found = []
        for directory in (github / "workflows", github / "actions"):
            for path in sorted(directory.rglob("*.yml")):
                content = path.read_text(encoding="utf-8")
                calls = list(
                    re.finditer(
                        r"uses: \./\.github/actions/prepare-bazel-ci\n( +)with:\n((?:\1  [^\n]*\n)+)",
                        content,
                    )
                )
                self.assertEqual(
                    len(calls),
                    content.count("uses: ./.github/actions/prepare-bazel-ci"),
                    str(path),
                )
                for call in calls:
                    params = call.group(2)
                    if directory.name == "workflows":
                        expected = "${{ secrets.BUILDBUDDY_API_KEY != '' }}"
                    elif path.parent.name == "run-argument-comment-lint":
                        expected = "${{ inputs.buildbuddy-api-key != '' }}"
                    else:
                        expected = "${{ inputs.remote-credentials }}"
                    self.assertIn("remote-credentials: " + expected, params, str(path))
                    found.append(path)
        self.assertTrue(found, "expected prepare-bazel-ci callers to audit")
        package = (github / "actions/build-codex-packages/action.yml").read_text(
            encoding="utf-8"
        )
        self.assertRegex(
            package,
            r"uses: \./\.github/actions/build-bazel-package-binaries\n +with:\n +remote-credentials: \$\{\{ inputs.remote-credentials \}\}",
        )

    def test_composite_boolean_defaults_and_both_phase_env_connections(self):
        actions = Path(__file__).parents[1] / "actions"
        for name in (
            "prepare-bazel-ci",
            "build-bazel-package-binaries",
            "build-codex-packages",
        ):
            content = (actions / name / "action.yml").read_text(encoding="utf-8")
            field = re.search(
                r"^  remote-credentials:\n(.*?)(?=^  [\w-]+:|^outputs:|^runs:)",
                content,
                re.M | re.S,
            ).group(1)
            self.assertIn('default: "false"', field)
            self.assertIn("required: false", field)
        prepare = (actions / "prepare-bazel-ci/action.yml").read_text(encoding="utf-8")
        steps = dict(
            re.findall(
                r"^    - name: ([^\n]+)\n(.*?)(?=^    - |\Z)", prepare, re.M | re.S
            )
        )
        for when in ("before", "after"):
            block = steps["Observe storage " + when + " repository restore"]
            self.assertIn(
                "HOSTED_STORAGE_REMOTE_CREDENTIALS: ${{ inputs.remote-credentials }}",
                block,
            )
            self.assertIn("observe " + when + "-restore none", block)
            self.assertIn("continue-on-error: true", block)
            self.assertNotIn("BUILDBUDDY_API_KEY:", block)

    def test_prepare_keyed_emits_nothing_and_keyless_retains_both_phases(self):
        for available in ("true", "false"):
            with (
                self.subTest(available=available),
                tempfile.TemporaryDirectory() as directory,
            ):
                step_output = Path(directory) / "output"
                env = dict(
                    ENV,
                    HOSTED_STORAGE_REMOTE_CREDENTIALS=available,
                    RUNNER_TEMP=directory,
                    GITHUB_OUTPUT=str(step_output),
                )
                with (
                    patch.dict(os.environ, env, clear=True),
                    patch.object(
                        storage,
                        "receipt",
                        return_value={"exit_code": None, "roots": {}},
                    ) as collect,
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    for phase in ("before-restore", "after-restore"):
                        with patch.object(
                            sys, "argv", ["script", "observe", phase, "none"]
                        ):
                            storage.main()
                if available == "true":
                    collect.assert_not_called()
                    self.assertFalse(step_output.exists())
                    self.assertFalse((Path(directory) / "hosted-storage").exists())
                else:
                    self.assertEqual(
                        [call.args[0] for call in collect.call_args_list],
                        ["before-restore", "after-restore"],
                    )
                    self.assertEqual(
                        step_output.read_text().splitlines(),
                        ["enabled=true", "enabled=true"],
                    )
                    self.assertEqual(
                        len(list((Path(directory) / "hosted-storage").glob("*.json"))),
                        2,
                    )


class ObservationTests(unittest.TestCase):
    def test_remote_credential_boolean_blocks_workflow_outputs_and_metrics(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "step-output"
            with (
                patch.dict(
                    os.environ,
                    dict(
                        ENV,
                        HOSTED_STORAGE_REMOTE_CREDENTIALS="true",
                        GITHUB_OUTPUT=str(output),
                    ),
                    clear=True,
                ),
                patch.object(
                    sys, "argv", ["script", "observe", "before-cache-save", "none"]
                ),
                patch.object(storage, "receipt") as collect,
            ):
                storage.main()
            collect.assert_not_called()
            self.assertFalse(output.exists())

    def test_step_output_failure_does_not_hide_numeric_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            result = {"exit_code": 37, "roots": {"output_base": {"available_bytes": 7}}}
            out = io.StringIO()
            with (
                patch.dict(
                    os.environ,
                    dict(ENV, RUNNER_TEMP=directory, GITHUB_OUTPUT=directory),
                    clear=True,
                ),
                patch.object(
                    sys, "argv", ["script", "observe", "after-build", "37", "test"]
                ),
                patch.object(storage, "receipt", return_value=result),
                contextlib.redirect_stdout(out),
            ):
                storage.main()
            self.assertIn('"output_base": 7', out.getvalue())
            self.assertEqual(
                len(list((Path(directory) / "hosted-storage").glob("*.json"))), 1
            )

    def test_numeric_capacity_and_bounded_du(self):
        fs = SimpleNamespace(
            f_blocks=100, f_bfree=30, f_bavail=20, f_frsize=4096, f_favail=12
        )
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(storage.os, "statvfs", return_value=fs, create=True),
            patch.object(
                storage.subprocess,
                "run",
                return_value=SimpleNamespace(returncode=0, stdout=b"4\tignored\n"),
            ) as run,
        ):
            result = storage.measure(Path(directory))
        self.assertEqual(result["available_bytes"], 81920)
        self.assertEqual(result["allocated_bytes"], 4096)
        self.assertEqual(run.call_args.kwargs["timeout"], 2)
        self.assertNotIn("-L", run.call_args.args[0])

    def test_du_timeout_is_explicit_not_zero(self):
        fs = SimpleNamespace(
            f_blocks=100, f_bfree=30, f_bavail=20, f_frsize=4096, f_favail=12
        )
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(storage.os, "statvfs", return_value=fs, create=True),
            patch.object(
                storage.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired("du", 2),
            ),
        ):
            result = storage.measure(Path(directory))
        self.assertTrue(result["allocation_unavailable"])
        self.assertNotIn("allocated_bytes", result)

    def test_aliases_overlaps_missing_and_no_environment_dump(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(
                ENV,
                BAZEL_OUTPUT_BASE=directory,
                BAZEL_REPOSITORY_CACHE=directory,
                TMP=str(Path(directory) / "tmp"),
                SECRET="must-not-appear",
            )
            with (
                patch.object(storage, "measure", return_value={"available_bytes": 42}),
                patch.object(storage.subprocess, "run", side_effect=OSError),
            ):
                result = storage.receipt(
                    "after-build", ["test", "--disk_cache="], env, 37
                )
        self.assertEqual(result["exit_code"], 37)
        self.assertEqual(result["roots"]["repository_cache"]["alias_of"], "output_base")
        self.assertIn("output_base", result["roots"]["ci_temp"]["overlaps"])
        self.assertTrue(result["roots"]["disk_cache"]["disabled_or_unavailable"])
        self.assertNotIn("must-not-appear", json.dumps(result))

    def test_storage_errors_never_replace_build_outcome_or_expose_error_text(self):
        out = io.StringIO()
        with (
            patch.dict(os.environ, ENV, clear=True),
            patch.object(
                sys, "argv", ["script", "observe", "after-build", "37", "test"]
            ),
            patch.object(storage, "receipt", side_effect=OSError("SECRET")),
            contextlib.redirect_stderr(out),
        ):
            storage.main()
        self.assertEqual(out.getvalue(), "Hosted storage observation unavailable\n")

    def test_numeric_summary_survives_receipt_disk_write_failure(self):
        out, errors = io.StringIO(), io.StringIO()
        result = {"exit_code": 37, "roots": {"output_base": {"available_bytes": 0}}}
        with (
            patch.dict(os.environ, dict(ENV, RUNNER_TEMP="unused"), clear=True),
            patch.object(
                sys, "argv", ["script", "observe", "after-build", "37", "test"]
            ),
            patch.object(storage, "receipt", return_value=result),
            patch.object(Path, "mkdir", side_effect=OSError("disk full")),
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(errors),
        ):
            storage.main()
        self.assertIn('"output_base": 0', out.getvalue())
        self.assertIn('"exit_code": 37', out.getvalue())
        self.assertIn("observation unavailable", errors.getvalue())


class ShellTests(unittest.TestCase):
    def invoke(self, flags=(), extra=()):
        bash = (
            str(
                Path(os.environ.get("ProgramFiles", "C:/Program Files"))
                / "Git/bin/bash.exe"
            )
            if os.name == "nt"
            else shutil.which("bash")
        )
        if not bash:
            self.skipTest("bash unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = Path(__file__).parent
            shutil.copyfile(source / "run-bazel-ci.sh", root / "run-bazel-ci.sh")
            shutil.copyfile(source / "hosted_storage.py", root / "policy.py")
            shutil.copyfile(
                source / "run_bazel_with_buildbuddy.py", root / "backend.py"
            )
            # Mock just filesystem observations; use real defaults/projection.
            (root / "hosted_storage.py").write_text(
                "import json, os, sys\nimport policy\n"
                "if sys.argv[1] in ('observe', 'resolve'):\n"
                " with open(os.environ['RECORD'], 'a') as f: f.write(json.dumps(sys.argv[2:4])+'\\n')\n"
                " if sys.argv[1] == 'resolve': sys.exit(41)\n"
                "else: policy.main()\n",
                encoding="utf-8",
            )
            (root / "run_bazel_with_buildbuddy.py").write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                "from backend import REMOTE_EXECUTION_CONFIGS, bazel_args_without_remote_execution, bazel_command\n"
                "if __name__ == '__main__':\n"
                " with open(os.environ['RECORD'], 'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n"
                " if 'info' in sys.argv: print('/nonexistent-testlogs'); sys.exit(0)\n"
                " sys.exit(37)\n",
                encoding="utf-8",
            )
            (root / "run_bazel_with_buildbuddy.py").chmod(0o755)
            record = root / "record"
            env = {
                key: value
                for key, value in os.environ.items()
                if key in ("PATH", "SYSTEMROOT", "SystemRoot", "TEMP", "TMP")
            }
            env.update(ENV, RECORD=str(record), MSYS2_ARG_CONV_EXCL="*")
            result = subprocess.run(
                [
                    bash,
                    str(root / "run-bazel-ci.sh"),
                    "--print-failed-test-logs",
                    "--",
                    "test",
                    *flags,
                    "--",
                    "//target",
                    *extra,
                ],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
            )
            return result, [
                json.loads(line) for line in record.read_text().splitlines()
            ]

    def test_failure_snapshot_precedes_info_and_original_exit_survives(self):
        result, calls = self.invoke(
            ["--compilation_mode", "opt", "--platforms", "//:custom"]
        )
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(calls[0], ["before-build", "none"])
        self.assertEqual(calls[2], ["after-build", "37"])
        self.assertEqual(calls[3], ["after-build-paths", "37"])
        self.assertIn("Hosted Bazel paths unavailable", result.stderr)
        self.assertIn("info", calls[4])
        self.assertIn("--disk_cache=", calls[1])
        self.assertIn("--disk_cache=", calls[4])
        self.assertIn("--compilation_mode=opt", calls[4])
        self.assertIn("--platforms=//:custom", calls[4])

    def test_explicit_split_cache_and_payload_are_not_changed(self):
        result, calls = self.invoke(
            ["--disk_cache", "cache with spaces"], ["--disk_cache=payload"]
        )
        self.assertEqual(result.returncode, 37, result.stderr)
        build = calls[1]
        self.assertNotIn("--disk_cache=", build)
        self.assertEqual(
            build[build.index("--") + 1 :], ["//target", "--disk_cache=payload"]
        )
        self.assertIn("--disk_cache=cache with spaces", calls[4])
        self.assertNotIn("--disk_cache=payload", calls[4])

    def test_both_remote_ci_spellings_are_removed_before_shell_dispatch(self):
        for name in ("ci-linux", "ci-macos", "ci-v8"):
            for flags in (["--config=" + name], ["--config", name]):
                result, calls = self.invoke(flags, ["--config=" + name])
                self.assertEqual(result.returncode, 37, result.stderr)
                build, info = calls[1], calls[4]
                self.assertIn("--disk_cache=", build)
                self.assertIn("--disk_cache=", info)
                self.assertNotIn("--config=" + name, build[: build.index("--")])
                self.assertNotIn(name, build[: build.index("--")])
                self.assertNotIn("--config=" + name, info)
                self.assertEqual(
                    build[build.index("--") + 1 :], ["//target", "--config=" + name]
                )


if __name__ == "__main__":
    unittest.main()
