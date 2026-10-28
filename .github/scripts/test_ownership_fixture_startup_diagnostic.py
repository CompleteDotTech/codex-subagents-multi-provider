"""Mock-only first-launch control checks; never starts PowerShell."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import types
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location(
    "startup", Path(__file__).with_name("diagnose_ownership_fixture_startup.py")
)
m = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(m)


class StartupTests(unittest.TestCase):
    def setUp(self):
        self.fixture = m.load_fixture()

    def invoke(self, mode, helper, stderr=None):
        output = stderr if stderr is not None else io.StringIO()
        with (
            patch.object(self.fixture.sys, "platform", "linux"),
            patch.object(
                self.fixture, "run_linux_ownership_fixture", side_effect=helper
            ),
            patch.object(m, "binary_digest", return_value="a" * 64),
            contextlib.redirect_stderr(output),
        ):
            m.run(mode, self.fixture)
        return output.getvalue()

    def test_minimal_single_same_argv_capture_and_posttrial_hash(self):
        events = []
        original = self.fixture.OwnershipFixtureCapture

        def helper(argv):
            events.append("trial")
            self.assertEqual(argv[:3], ["pwsh", "-NoProfile", "-File"])
            self.assertEqual(len(argv), 5)
            self.assertEqual(Path(argv[3]).name, "fixture.ps1")
            self.assertEqual(Path(argv[3]).read_text(), m.MINIMAL)
            capture = self.fixture.OwnershipFixtureCapture()
            self.assertIsInstance(capture, original)
            self.assertIs(type(capture).feed, original.feed)
            capture.feed("stderr", b"OWNERSHIP_FIXTURE_ENTRY\n", 123)
            return subprocess.CompletedProcess(argv, 0, "{}", "")

        with (
            patch.object(m, "binary_digest", side_effect=lambda: events.append("hash")),
            patch.object(
                self.fixture, "run_linux_ownership_fixture", side_effect=helper
            ),
            contextlib.redirect_stderr(io.StringIO()) as output,
        ):
            m.run("minimal", self.fixture)
        self.assertEqual(events, ["trial", "hash"])
        facts = json.loads(output.getvalue().split(" ", 1)[1])
        self.assertEqual(facts["parent_observed_first_entry_ms"], 123)
        self.assertTrue(facts["control_only"])

    def test_full_actual_field_assertions_are_not_bypassed(self):
        # Invalid successful child JSON reaches the original method and fails its field oracle.
        error = None
        try:
            self.invoke(
                "full", lambda argv: subprocess.CompletedProcess(argv, 0, "{}", "")
            )
        except KeyError as caught:
            error = caught
        self.assertIsNotNone(error)

    def test_timeout_identity_survives_summary_failure_and_late_valid_data(self):
        primary = subprocess.TimeoutExpired(["SECRET"], 10)
        primary.ownership_fixture_cleanup = {"cleanup_complete": True}
        tracebacks = []

        def helper(argv):
            capture = self.fixture.OwnershipFixtureCapture()
            capture.feed("stdout", b"{}", 12000)
            try:
                raise primary
            except BaseException as error:
                tracebacks.append(error.__traceback__)
                raise

        broken = Mock()
        broken.write.side_effect = OSError("SECRET")
        for mode in ("minimal", "full"):
            with self.subTest(mode=mode):
                primary.__traceback__ = None
                try:
                    self.invoke(mode, helper, broken)
                except BaseException as error:
                    self.assertIs(error, primary)
                    tb = error.__traceback__
                    while tb.tb_next:
                        tb = tb.tb_next
                    self.assertIs(tb, tracebacks[-1])
                    self.assertFalse(
                        error.ownership_startup_cleanup["cleanup_complete"]
                    )
                else:
                    self.fail("late valid output must remain failure")

    def test_no_primary_summary_failure_fails_closed(self):
        broken = Mock()
        broken.write.side_effect = OSError("SECRET")
        with self.assertRaises(OSError) as result:
            self.invoke(
                "minimal",
                lambda argv: subprocess.CompletedProcess(argv, 0, "{}", ""),
                broken,
            )
        self.assertTrue(
            result.exception.ownership_startup_cleanup["retain_workload_lease"]
        )

    def test_interrupt_and_temporary_cleanup_preserve_primary(self):
        primary = KeyboardInterrupt()
        temporary = types.SimpleNamespace(
            name="unused", cleanup=Mock(side_effect=OSError("SECRET"))
        )
        with (
            patch.object(m.tempfile, "TemporaryDirectory", return_value=temporary),
            patch.object(m.Path, "write_text"),
            patch.object(m, "binary_digest", return_value=None),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            with patch.object(
                self.fixture, "run_linux_ownership_fixture", side_effect=primary
            ):
                try:
                    m.run("minimal", self.fixture)
                except BaseException as error:
                    self.assertIs(error, primary)
                    self.assertEqual(
                        error.ownership_startup_cleanup["failed_phases"],
                        ["temporary_cleanup"],
                    )
                else:
                    self.fail("interrupt lost")

    def test_summary_redacts_raw_capture_and_reports_bounded_version(self):
        capture = self.fixture.OwnershipFixtureCapture()
        capture.feed(
            "stderr",
            b"SECRET /private/path\nOWNERSHIP_FIXTURE_ENTRY\nOWNERSHIP_FIXTURE stage=entered elapsed_ms=1 pwsh_major=7 pwsh_minor=6\n",
            321,
        )
        facts = m.summary(self.fixture, "minimal", [capture], None, "a" * 64)
        encoded = json.dumps(facts)
        self.assertNotIn("SECRET", encoded)
        self.assertNotIn("/private", encoded)
        self.assertEqual(facts["pwsh_version"], [7, 6])
        self.assertEqual(facts["parent_observed_first_stage_ms"], {"entered": 321})

    def test_no_primary_temporary_cleanup_failure_fails_closed(self):
        temporary = types.SimpleNamespace(
            name="unused", cleanup=Mock(side_effect=OSError("SECRET"))
        )
        with (
            patch.object(m.tempfile, "TemporaryDirectory", return_value=temporary),
            patch.object(m.Path, "write_text"),
            patch.object(m, "binary_digest", return_value=None),
            contextlib.redirect_stderr(io.StringIO()),
            patch.object(
                self.fixture,
                "run_linux_ownership_fixture",
                return_value=subprocess.CompletedProcess([], 0, "{}", ""),
            ),
        ):
            with self.assertRaises(OSError) as result:
                m.run("minimal", self.fixture)
        self.assertFalse(result.exception.ownership_startup_cleanup["cleanup_complete"])

    def test_unknown_mode_has_no_trial_or_metadata(self):
        with patch.object(m, "binary_digest") as digest:
            with self.assertRaises(ValueError):
                m.run("SECRET", self.fixture)
            digest.assert_not_called()


if __name__ == "__main__":
    unittest.main()
