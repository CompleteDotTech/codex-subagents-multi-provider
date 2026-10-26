#!/usr/bin/env python3
"""Fast failure-path checks; these tests never invoke Cargo or crypto builds."""

import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "noise_interop_driver", Path(__file__).with_name("test_noise_interop.py")
)
DRIVER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRIVER)


class DriverFailureTests(unittest.TestCase):
    def test_second_start_failure_reaps_first_endpoint(self):
        child = mock.Mock()
        child.poll.return_value = None
        with mock.patch.object(
            DRIVER.subprocess, "Popen", side_effect=[child, OSError("second failed")]
        ):
            with self.assertRaisesRegex(OSError, "second failed"):
                DRIVER.exchange(Path("old"), Path("missing"))
        child.kill.assert_called_once()
        child.wait.assert_called_once()

    def test_cargo_artifact_path_overrides_guessed_layout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            actual = root / "configured target with spaces" / "host" / "endpoint"
            actual.parent.mkdir(parents=True)
            actual.write_bytes(b"test artifact")
            result = subprocess.CompletedProcess(
                [],
                0,
                stdout=json.dumps(
                    {
                        "reason": "compiler-artifact",
                        "target": {"name": "noise-interop-new"},
                        "executable": str(actual),
                    }
                )
                + "\n",
            )
            with mock.patch.object(DRIVER.subprocess, "run", return_value=result):
                self.assertEqual(
                    DRIVER.project(root, "new", "unused-cargo"), actual.resolve()
                )

    def test_missing_cargo_artifact_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = subprocess.CompletedProcess([], 0, stdout="")
            with mock.patch.object(DRIVER.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(AssertionError, "exactly one"):
                    DRIVER.project(Path(temporary), "new", "unused-cargo")

    def test_protocol_eof_fails_closed(self):
        child = mock.Mock(stdout=io.StringIO(""))
        with self.assertRaisesRegex(AssertionError, "stopped"):
            DRIVER.read(child)


if __name__ == "__main__":
    unittest.main()
