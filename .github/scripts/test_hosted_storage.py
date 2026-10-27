"""No-building tests for storage policy, bounded observations and shell wiring."""

import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import hosted_storage as storage

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
            ["--config=ci-linux"],
            ["--strategy=remote"],
        ):
            self.assertEqual(storage.defaults(["test", *flags], ENV), [])

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


class ObservationTests(unittest.TestCase):
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
            # Mock just filesystem observations; use real defaults/projection.
            (root / "hosted_storage.py").write_text(
                "import json, os, sys\nimport policy\n"
                "if sys.argv[1] == 'observe':\n"
                " with open(os.environ['RECORD'], 'a') as f: f.write(json.dumps(sys.argv[2:4])+'\\n')\n"
                "else: policy.main()\n",
                encoding="utf-8",
            )
            (root / "run_bazel_with_buildbuddy.py").write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                "with open(os.environ['RECORD'], 'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n"
                "if 'info' in sys.argv: print('/nonexistent-testlogs'); sys.exit(0)\n"
                "sys.exit(37)\n",
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
        self.assertIn("info", calls[3])
        self.assertIn("--disk_cache=", calls[1])
        self.assertIn("--disk_cache=", calls[3])
        self.assertIn("--compilation_mode=opt", calls[3])
        self.assertIn("--platforms=//:custom", calls[3])

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
        self.assertIn("--disk_cache=cache with spaces", calls[3])
        self.assertNotIn("--disk_cache=payload", calls[3])


if __name__ == "__main__":
    unittest.main()
