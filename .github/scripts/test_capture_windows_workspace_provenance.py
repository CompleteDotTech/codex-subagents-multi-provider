"""Verify provenance capture preserves observed bytes without granting authority."""

import importlib.util
import json
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
SPEC = importlib.util.spec_from_file_location(
    "capture", Path(__file__).with_name("capture-windows-workspace-provenance.py")
)
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)


class CaptureTest(unittest.TestCase):
    def test_capture_preserves_failed_status_before_canonicalization(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            base, output = root / "base", root / "logs"
            status = base / "execroot/_main/bazel-out/stable-status.txt"
            status.parent.mkdir(parents=True)
            failed = b"BUILD_HOST runner\n"
            status.write_bytes(failed)
            env = {
                "RUNNER_OS": "Windows",
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": capture.FORK,
                "BAZEL_OUTPUT_BASE": str(base),
                "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR": str(output),
            }

            def run(argv, cwd):
                if argv[0] == "git":
                    return 0, b"a" * 40 + b"\n", b""
                if argv[0] == "CMD.EXE":
                    return 0, b"STABLE_GIT_COMMIT " + b"a" * 40 + b"\r\n", b""
                # A later Bazel call must not replace the captured failed bytes.
                status.write_bytes(b"changed by later command\n")
                return (
                    0,
                    b"--workspace_status_command=unknown\n--remote_header=secret\n",
                    b"sensitive error",
                )

            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(capture, "command", side_effect=run),
                patch.object(capture, "bazel_command", return_value=["bazel"]),
                patch.object(sys, "argv", ["capture", "build", "--config=clippy"]),
            ):
                capture.main()
            destination = output / "workspace-provenance" / str(os.getpid())
            self.assertEqual((destination / "stable-status.txt").read_bytes(), failed)
            report = (destination / "observation.json").read_text()
            self.assertNotIn("secret", report)
            self.assertNotIn("sensitive", report)
            self.assertEqual(json.loads(report)["head"]["value"], "a" * 40)

    def test_test_flags_and_failed_canonicalization_still_replay_known_producer(self):
        with TemporaryDirectory() as directory:
            output = Path(directory)
            env = {
                "RUNNER_OS": "Windows",
                "GITHUB_ACTIONS": "true",
                "GITHUB_REPOSITORY": capture.FORK,
                "BAZEL_OUTPUT_BASE": str(output / "missing"),
                "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR": str(output),
            }

            def run(argv, cwd):
                if argv[0] == "git":
                    return 0, b"b" * 40, b""
                if argv[0] == "CMD.EXE":
                    return 0, b"STABLE_GIT_COMMIT " + b"b" * 40, b""
                return 2, b"--remote_header=secret", b"Unrecognized option: secret"

            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(capture, "command", side_effect=run) as commands,
                patch.object(capture, "bazel_command", return_value=["bazel"]) as bazel,
                patch.object(
                    sys, "argv", ["capture", "test", "--test_verbose_timeout_warnings"]
                ),
            ):
                capture.main()
            bazel.assert_called_once_with(
                "canonicalize-flags",
                "--for_command=test",
                "--",
                "--test_verbose_timeout_warnings",
            )
            self.assertEqual(
                commands.call_args.args[0],
                [
                    "CMD.EXE",
                    "/S",
                    "/E:ON",
                    "/V:ON",
                    "/D",
                    "/C",
                    "scripts\\workspace-status.cmd",
                ],
            )
            destination = output / "workspace-provenance" / str(os.getpid())
            report = (destination / "observation.json").read_text()
            self.assertNotIn("secret", report)
            data = json.loads(report)
            self.assertEqual(
                data["effective_flags"]["failure_category"], "unknown_option"
            )
            self.assertFalse(data["ctx.info_file"]["present"])
            self.assertTrue(
                (destination / "status-producer-stdout.bin")
                .read_bytes()
                .startswith(b"STABLE_GIT_COMMIT ")
            )

    def test_scope_and_file_limits(self):
        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(capture, "command") as run,
        ):
            capture.main()
            run.assert_not_called()
        with TemporaryDirectory() as directory:
            source, destination = (
                Path(directory) / "source",
                Path(directory) / "destination",
            )
            source.write_bytes(b"1234")
            self.assertFalse(capture.capture_file(source, destination, 3)["captured"])
            self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
