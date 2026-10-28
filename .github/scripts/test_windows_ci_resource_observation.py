"""Fixture-only checks: no host CIM queries or actual builds."""

import contextlib
import importlib.util
import io
import json
import locale
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import tempfile
import sys
import threading
import time
import textwrap
import unittest
from unittest.mock import patch, Mock

SPEC = importlib.util.spec_from_file_location(
    "observation", Path(__file__).with_name("observe_windows_ci_resources.py")
)
m = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(m)


def ownership_capture_metadata(value):
    """Count only bounded bytes; text counts use UTF-8 replacement encoding."""
    if isinstance(value, bytes):
        size = len(value)
        kind = "bytes"
    elif isinstance(value, str):
        # At most 8193 characters are encoded, even for an oversized capture.
        size = len(value[:8193].encode("utf-8", errors="replace"))
        kind = "text"
    else:
        return {
            "present": value is not None,
            "kind": "none" if value is None else "unsupported",
            "byte_count": None,
            "byte_count_clamped": False,
        }
    return {
        "present": True,
        "kind": kind,
        "byte_count": min(size, 8192),
        "byte_count_clamped": size > 8192,
    }


def ownership_fixture_timeout_summary(stderr, stdout=None):
    """Retain only bounded fixed-stage/numeric observations, never raw errors."""
    stages = {
        "entered",
        "mocks-ready",
        "collector-call",
        "collector-return",
        "query-memory",
        "query-pagefiles",
        "query-settings",
        "query-volume",
        "query-process",
        "vhd-test",
        "vhd-item",
    }
    captures = {
        "stdout_capture": ownership_capture_metadata(stdout),
        "stderr_capture": ownership_capture_metadata(stderr),
    }
    if isinstance(stderr, bytes):
        stderr = stderr[-8192:].decode("utf-8", errors="replace")
    elif isinstance(stderr, str):
        stderr = stderr[-8192:]
    else:
        stderr = ""
    observed = []
    elapsed = None
    version = None
    retained_lines = stderr.splitlines()[-64:]
    for line in retained_lines:
        match = re.fullmatch(
            r"OWNERSHIP_FIXTURE stage=([a-z-]+) elapsed_ms=([0-9]{1,5}) "
            r"pwsh_major=([0-9]{1,3}) pwsh_minor=([0-9]{1,3})",
            line,
        )
        if not match or match[1] not in stages:
            continue
        milliseconds, major, minor = map(int, match.group(2, 3, 4))
        if milliseconds > 60000 or major > 100 or minor > 100:
            continue
        if not observed or observed[-1] != match[1]:
            observed.append(match[1])
        elapsed, version = milliseconds, [major, minor]
    return "ownership-fixture-timeout " + json.dumps(
        {
            "stages": observed[-16:],
            "elapsed_ms": elapsed,
            "pwsh_version": version,
            "literal_entry_seen_in_retained_stderr": "OWNERSHIP_FIXTURE_ENTRY"
            in retained_lines,
            "capture_byte_count_limit": 8192,
            "text_byte_count_encoding": "utf8-replace",
            "stderr_window_limit": 8192,
            "stderr_window_units": "bytes-before-utf8-decode-or-text-characters",
            "stderr_line_limit": 64,
            **captures,
        },
        separators=(",", ":"),
    )


class OwnershipFixtureCapture:
    """Bounded byte capture and parent-observed fixed-line arrival times."""

    def __init__(self):
        self.stdout = bytearray()
        self.stderr = bytearray()
        self.stderr_tail = bytearray()
        self.counts = {"stdout": 0, "stderr": 0}
        self.line = bytearray()
        self.long_line = False
        self.entry_ms = None
        self.stages = {}

    def feed(self, role, block, elapsed_ms):
        self.counts[role] = min(1024 * 1024 + 1, self.counts[role] + len(block))
        if self.counts[role] > 1024 * 1024:
            raise OverflowError("ownership fixture capture exceeded bound")
        if role == "stdout":
            if len(self.stdout) + len(block) > m.MAX_SAMPLE:
                raise OverflowError("ownership fixture JSON exceeded bound")
            self.stdout.extend(block)
            return
        self.stderr.extend(block)
        self.stderr_tail.extend(block)
        del self.stderr_tail[:-8192]
        for part in block.splitlines(keepends=True):
            self.line.extend(part[:513])
            if len(self.line) > 512:
                self.long_line = True
                self.line.clear()
            if part.endswith(b"\n"):
                if not self.long_line:
                    value = bytes(self.line).rstrip(b"\r\n")
                    if value == b"OWNERSHIP_FIXTURE_ENTRY" and self.entry_ms is None:
                        self.entry_ms = elapsed_ms
                    parsed = json.loads(
                        ownership_fixture_timeout_summary(value).split(" ", 1)[1]
                    )
                    for stage in parsed["stages"]:
                        self.stages.setdefault(stage, elapsed_ms)
                self.line.clear()
                self.long_line = False


def ownership_fixture_birth(pid):
    # Linux only. This identity remains private; never emitted in diagnostics.
    value = Path(f"/proc/{pid}/stat").read_text()
    return int(value[value.rfind(")") + 2 :].split()[19])


def record_ownership_fixture_secondary_failure(primary, phase):
    """Fixed failure facts only; preserve the existing primary and owned lease."""
    allowed = {
        "exit_observation",
        "selector_close",
        "stdout_close",
        "stderr_close",
        "group_cleanup",
        "diagnostic_write",
        "caller_diagnostic_write",
        "fixture_temp_cleanup",
    }
    previous = getattr(primary, "ownership_fixture_cleanup", None)
    phases = previous.get("failed_phases", []) if isinstance(previous, dict) else []
    phases = [value for value in phases if isinstance(value, str) and value in allowed]
    if phase in allowed and phase not in phases:
        phases.append(phase)
    primary.ownership_fixture_cleanup = {
        "cleanup_complete": False,
        "failed_phases": phases,
        "retain_workload_lease": True,
    }


def cleanup_ownership_fixture(child, birth):
    """Only the private child group; bounded failure cleanup, no pipe drain."""

    def exists():
        try:
            os.killpg(child.pid, 0)
            return True
        except ProcessLookupError:
            return False

    def stop(sig):
        try:
            current = ownership_fixture_birth(child.pid)
        except FileNotFoundError:
            current = None
        if current is not None and (birth is None or current != birth):
            raise RuntimeError("ownership fixture leader identity unavailable")
        try:
            os.killpg(child.pid, sig)
        except ProcessLookupError:
            pass

    def settle(seconds):
        cutoff = time.monotonic() + seconds
        while True:
            child.poll()  # Nonblocking reap, including a terminated group leader.
            if not exists():
                return True
            if time.monotonic() >= cutoff:
                return False
            time.sleep(min(0.01, max(0, cutoff - time.monotonic())))

    if child.pid <= 1 or child.pid == os.getpgrp():
        raise RuntimeError("ownership fixture has no private process group")
    if exists():
        stop(signal.SIGTERM)
        if not settle(1):
            stop(signal.SIGKILL)
            settle(2)
    child.poll()
    if child.returncode is None or exists():
        raise RuntimeError("ownership fixture cleanup incomplete")


def run_linux_ownership_fixture(argv):
    """Same 10s qualification; at most 20s SAME-child failure-only observation.

    Process creation may block in OS APIs. No hostile-host/escaped-process or
    arbitrary scheduling bound is claimed. Windows keeps subprocess.run.
    """
    child = None
    birth = None
    selector = None
    capture = OwnershipFixtureCapture()
    timed_out = None
    live_at_cutoff = None
    exit_category = "unavailable"
    cleanup_complete = False
    primary = None
    primary_traceback = None
    result = None
    cleanup_errors = []

    def attempt(phase, callback):
        # Fixed phase labels only. Keep private exception objects out of output.
        try:
            callback()
        except BaseException as error:
            cleanup_errors.append((phase, error, error.__traceback__))

    try:
        child = subprocess.Popen(
            argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True
        )
        started = time.monotonic()
        deadline = started + 10
        observation_deadline = deadline + 20
        birth = ownership_fixture_birth(child.pid)
        if os.getpgid(child.pid) != child.pid or os.getsid(child.pid) != child.pid:
            raise RuntimeError("ownership fixture private session unavailable")
        selector = selectors.DefaultSelector()
        for role, pipe in (("stdout", child.stdout), ("stderr", child.stderr)):
            os.set_blocking(pipe.fileno(), False)
            selector.register(pipe, selectors.EVENT_READ, role)
        while True:
            now = time.monotonic()
            if timed_out is None and now >= deadline:
                timed_out = subprocess.TimeoutExpired(
                    argv,
                    10,
                    output=bytes(capture.stdout),
                    stderr=bytes(capture.stderr_tail),
                )
                live_at_cutoff = child.poll() is None
            if timed_out is not None and now >= observation_deadline:
                break
            code = child.poll()
            if not selector.get_map() and code is not None:
                break
            cutoff = deadline if timed_out is None else observation_deadline
            for key, _ in selector.select(timeout=min(0.05, max(0, cutoff - now))):
                # A read-ready wakeup can itself be scheduled after a cutoff.
                # Freeze the original capture BEFORE consuming any late bytes.
                observed_at = time.monotonic()
                if timed_out is None and observed_at >= deadline:
                    timed_out = subprocess.TimeoutExpired(
                        argv,
                        10,
                        output=bytes(capture.stdout),
                        stderr=bytes(capture.stderr_tail),
                    )
                    live_at_cutoff = child.poll() is None
                if timed_out is not None and observed_at >= observation_deadline:
                    break
                try:
                    block = os.read(key.fd, 4096)
                except BlockingIOError:
                    continue
                if not block:
                    selector.unregister(key.fileobj)
                    continue
                elapsed = min(30000, max(0, int((time.monotonic() - started) * 1000)))
                capture.feed(key.data, block, elapsed)
        # Recheck clock even when child exit/EOF races the qualification cutoff.
        if timed_out is None and time.monotonic() >= deadline:
            timed_out = subprocess.TimeoutExpired(
                argv,
                10,
                output=bytes(capture.stdout),
                stderr=bytes(capture.stderr_tail),
            )
            live_at_cutoff = child.poll() is None
        if timed_out is not None:
            raise timed_out
        encoding = "utf-8" if sys.flags.utf8_mode else locale.getencoding()
        stdout = (
            bytes(capture.stdout)
            .decode(encoding)
            .replace("\r\n", "\n")
            .replace("\r", "\n")
        )
        stderr = (
            bytes(capture.stderr)
            .decode(encoding)
            .replace("\r\n", "\n")
            .replace("\r", "\n")
        )
        if child.returncode:
            raise subprocess.CalledProcessError(child.returncode, argv, stdout, stderr)
        result = subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)
    except BaseException as error:
        primary = (
            timed_out
            if timed_out is not None and isinstance(error, Exception)
            else error
        )
        primary_traceback = primary.__traceback__
    finally:
        if child is not None:

            def observe_exit():
                nonlocal exit_category
                observed_code = child.poll()
                exit_category = (
                    "still_live"
                    if observed_code is None
                    else "zero"
                    if observed_code == 0
                    else "nonzero"
                )

            attempt("exit_observation", observe_exit)
        if selector is not None:
            attempt("selector_close", selector.close)
        if child is not None:
            for role, pipe in (
                ("stdout_close", child.stdout),
                ("stderr_close", child.stderr),
            ):
                if pipe is not None:
                    attempt(role, pipe.close)
            attempt("group_cleanup", lambda: cleanup_ownership_fixture(child, birth))
        cleanup_complete = not cleanup_errors
        facts = {
            "cleanup_complete": cleanup_complete,
            "failed_phases": [phase for phase, _, _ in cleanup_errors],
            "retain_workload_lease": not cleanup_complete,
        }

        def emit_diagnostic():
            if timed_out is not None:
                print(
                    "ownership-fixture-late-observation "
                    + json.dumps(
                        {
                            "qualification_timeout_seconds": 10,
                            "failure_only_observation_seconds": 20,
                            "parent_observed_first_entry_ms": capture.entry_ms,
                            "parent_observed_first_stage_ms": capture.stages,
                            "child_live_at_qualification_cutoff": live_at_cutoff,
                            "exit_category": exit_category,
                            "cleanup_complete": cleanup_complete,
                            "cleanup": facts,
                            "late_known_markers": json.loads(
                                ownership_fixture_timeout_summary(
                                    bytes(capture.stderr_tail), bytes(capture.stdout)
                                ).split(" ", 1)[1]
                            ),
                            "observed_stream_byte_counts_clamped_8192": {
                                key: min(value, 8192)
                                for key, value in capture.counts.items()
                            },
                        },
                        separators=(",", ":"),
                    ),
                    file=sys.stderr,
                )
            elif cleanup_errors:
                print(
                    "ownership-fixture-cleanup "
                    + json.dumps(facts, separators=(",", ":")),
                    file=sys.stderr,
                )

        attempt("diagnostic_write", emit_diagnostic)
        # If stderr itself failed, the exception still retains fixed private
        # cleanup facts. Never replace the primary object with a close/write error.
        if cleanup_errors:
            facts.update(
                cleanup_complete=False,
                failed_phases=[phase for phase, _, _ in cleanup_errors],
                retain_workload_lease=True,
            )
        if primary is not None:
            primary.ownership_fixture_cleanup = facts
    if primary is not None:
        raise primary.with_traceback(primary_traceback)
    if cleanup_errors:
        _, error, error_traceback = cleanup_errors[0]
        error.ownership_fixture_cleanup = facts
        raise error.with_traceback(error_traceback)
    return result


class ObservationTests(unittest.TestCase):
    def linux_fixture_simulation(
        self,
        events,
        exit_at,
        code=0,
        read_error=None,
        close_error=None,
        pipe_error=None,
        group_error=None,
        diagnostic_error=None,
    ):
        """Exercise the Linux lifecycle with fake clock/fds; no child is started."""
        clock = [0.0]
        child = Mock(pid=12345, returncode=None)
        pipes = {"stdout": Mock(), "stderr": Mock()}
        child.stdout, child.stderr = pipes.values()
        for number, pipe in enumerate(pipes.values(), 11):
            pipe.fileno.return_value = number
        active = {}
        blocks = {}
        pending = list(events)
        selector = Mock()

        def register(pipe, mask, role):
            active[role] = Mock(fd=pipe.fileno(), fileobj=pipe, data=role)

        def unregister(pipe):
            del active[next(role for role in active if pipes[role] is pipe)]

        def select(timeout):
            until = clock[0] + timeout
            if pending and pending[0][0] <= until:
                when, role, block = pending.pop(0)
                clock[0] = max(clock[0], when)
                blocks[pipes[role].fileno()] = block
                return [(active[role], 1)]
            clock[0] = until
            return []

        def poll():
            if clock[0] >= exit_at:
                child.returncode = code
            return child.returncode

        child.poll.side_effect = poll
        selector.register.side_effect = register
        selector.unregister.side_effect = unregister
        selector.select.side_effect = select
        selector.get_map.side_effect = lambda: active
        selector.close.side_effect = close_error
        pipes["stderr"].close.side_effect = pipe_error
        output = io.StringIO()
        if diagnostic_error is not None:

            class BrokenOutput(io.StringIO):
                def write(self, value):
                    raise diagnostic_error

            output = BrokenOutput()
        with (
            patch.object(subprocess, "Popen", return_value=child) as launch,
            patch.object(selectors, "DefaultSelector", return_value=selector),
            patch.object(time, "monotonic", side_effect=lambda: clock[0]),
            patch.object(os, "set_blocking"),
            patch.object(os, "getpgid", return_value=child.pid, create=True),
            patch.object(os, "getsid", return_value=child.pid, create=True),
            patch.object(
                os, "read", side_effect=read_error or (lambda fd, size: blocks.pop(fd))
            ),
            patch(__name__ + ".ownership_fixture_birth", return_value=42),
            patch(
                __name__ + ".cleanup_ownership_fixture", side_effect=group_error
            ) as cleanup,
            contextlib.redirect_stderr(output),
        ):
            try:
                result = run_linux_ownership_fixture(["pwsh", "FIXTURE"])
                error = None
            except BaseException as caught:
                result, error = None, caught
        self.assertEqual(cleanup.call_count, 1)
        cleanup.assert_called_with(child, 42)
        selector.close.assert_called_once()
        for pipe in pipes.values():
            pipe.close.assert_called_once()
        launch.assert_called_once_with(
            ["pwsh", "FIXTURE"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        return result, error, output.getvalue(), clock[0]

    def test_linux_fixture_late_valid_json_is_same_irreversible_timeout(self):
        original = subprocess.TimeoutExpired
        errors = []

        class RecordedTimeout(original):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                errors.append(self)

        with patch.object(subprocess, "TimeoutExpired", RecordedTimeout):
            result, error, output, elapsed = self.linux_fixture_simulation(
                [
                    (12, "stderr", b"OWNERSHIP_FIXTURE_EN"),
                    (12.1, "stderr", b"TRY\nSECRET\n"),
                    (
                        12.2,
                        "stderr",
                        b"OWNERSHIP_FIXTURE stage=entered elapsed_ms=1 pwsh_major=7 pwsh_minor=4\n",
                    ),
                    (13, "stdout", b'{"valid":true}'),
                    (13.1, "stdout", b""),
                    (13.1, "stderr", b""),
                ],
                exit_at=13,
            )
        self.assertIsNone(result)
        self.assertIs(error, errors[0])
        self.assertEqual(len(errors), 1)
        self.assertEqual(error.timeout, 10)
        self.assertEqual(
            error.stdout, b""
        )  # Snapshot at10, never replaced by late JSON.
        data = json.loads(output.split(" ", 1)[1])
        self.assertEqual(data["exit_category"], "zero")
        self.assertTrue(data["child_live_at_qualification_cutoff"])
        self.assertEqual(data["parent_observed_first_entry_ms"], 12100)
        self.assertEqual(data["parent_observed_first_stage_ms"], {"entered": 12200})
        self.assertEqual(data["late_known_markers"]["pwsh_version"], [7, 4])
        self.assertNotIn("SECRET", output)
        self.assertNotIn("valid", output)
        self.assertLess(elapsed, 30)

    def test_linux_fixture_no_output_stops_observation_at_fixed_thirty(self):
        result, error, output, elapsed = self.linux_fixture_simulation([], exit_at=100)
        self.assertIsNone(result)
        self.assertIsInstance(error, subprocess.TimeoutExpired)
        self.assertLessEqual(elapsed, 30.001)
        data = json.loads(output.split(" ", 1)[1])
        self.assertIsNone(data["parent_observed_first_entry_ms"])
        self.assertEqual(data["parent_observed_first_stage_ms"], {})
        self.assertEqual(data["exit_category"], "still_live")

    def test_linux_fixture_late_overflow_cannot_replace_frozen_timeout(self):
        _, error, output, _ = self.linux_fixture_simulation(
            [(11, "stdout", b"x" * (m.MAX_SAMPLE + 1))], exit_at=100
        )
        self.assertIsInstance(error, subprocess.TimeoutExpired)
        self.assertEqual(error.timeout, 10)
        self.assertEqual(error.stdout, b"")
        self.assertTrue(json.loads(output.split(" ", 1)[1])["cleanup_complete"])

    def test_linux_fixture_drains_both_pipes_and_preserves_early_check_errors(self):
        for code in (0, 37):
            result, error, output, _ = self.linux_fixture_simulation(
                [
                    (1, "stdout", b'{"ok":true}\r\n'),
                    (1, "stderr", b"diagnostic\r\n"),
                    (1.1, "stdout", b""),
                    (1.1, "stderr", b""),
                ],
                exit_at=1,
                code=code,
            )
            self.assertEqual(output, "")
            observed = result if code == 0 else error
            self.assertEqual(observed.stdout, '{"ok":true}\n')
            self.assertEqual(observed.stderr, "diagnostic\n")
            self.assertEqual(observed.returncode, code)
            if code:
                self.assertIsInstance(error, subprocess.CalledProcessError)

    def test_linux_fixture_interrupt_and_read_error_close_all_owned_resources(self):
        for failure in (KeyboardInterrupt(), OSError("SECRET")):
            _, error, output, _ = self.linux_fixture_simulation(
                [(1, "stdout", b"x")], exit_at=100, read_error=failure
            )
            self.assertIs(error, failure)
            self.assertEqual(output, "")

    def test_linux_fixture_selector_close_error_still_closes_pipes_and_reaps(self):
        failure = OSError("SECRET close failure")
        _, error, output, _ = self.linux_fixture_simulation(
            [(1, "stdout", b""), (1, "stderr", b"")], exit_at=1, close_error=failure
        )
        self.assertIs(error, failure)
        facts = json.loads(output.split(" ", 1)[1])
        self.assertFalse(facts["cleanup_complete"])
        self.assertTrue(facts["retain_workload_lease"])

    def test_linux_fixture_timeout_survives_each_cleanup_and_diagnostic_failure(self):
        original = subprocess.TimeoutExpired
        for option, phase in (
            ("close_error", "selector_close"),
            ("pipe_error", "stderr_close"),
            ("group_error", "group_cleanup"),
            ("diagnostic_error", "diagnostic_write"),
        ):
            errors = []

            class RecordedTimeout(original):
                def __init__(self, *args, **kwargs):
                    super().__init__(*args, **kwargs)
                    errors.append(self)

            with patch.object(subprocess, "TimeoutExpired", RecordedTimeout):
                _, error, output, _ = self.linux_fixture_simulation(
                    [], exit_at=100, **{option: OSError("SECRET cleanup failure")}
                )
            self.assertIs(error, errors[0])
            self.assertEqual(len(errors), 1)
            facts = error.ownership_fixture_cleanup
            self.assertFalse(facts["cleanup_complete"])
            self.assertTrue(facts["retain_workload_lease"])
            self.assertIn(phase, facts["failed_phases"])
            self.assertNotIn("SECRET", output)
            self.assertNotIn("SECRET", json.dumps(facts))

    def test_linux_fixture_original_read_and_interrupt_traceback_survive_cleanup(self):
        for failure in (OSError("SECRET read"), KeyboardInterrupt("SECRET interrupt")):
            for option in (
                "close_error",
                "pipe_error",
                "group_error",
                "diagnostic_error",
            ):
                secondary = {option: RuntimeError("SECRET secondary")}
                if option == "diagnostic_error":
                    secondary["close_error"] = OSError("SECRET close")
                try:
                    raise failure
                except BaseException:
                    original_traceback = failure.__traceback__
                _, error, output, _ = self.linux_fixture_simulation(
                    [(1, "stdout", b"x")],
                    exit_at=100,
                    read_error=failure,
                    **secondary,
                )
                self.assertIs(error, failure)
                self.assertTrue(
                    error.ownership_fixture_cleanup["retain_workload_lease"]
                )
                frames = []
                original_tail_preserved = False
                tb = error.__traceback__
                while tb is not None:
                    original_tail_preserved |= tb is original_traceback
                    frames.append(tb.tb_frame.f_code.co_name)
                    tb = tb.tb_next
                self.assertTrue(original_tail_preserved)
                self.assertIn("run_linux_ownership_fixture", frames)
                self.assertIn("_execute_mock_call", frames)
                self.assertNotIn("SECRET", output)

    def test_linux_fixture_cleanup_failure_without_primary_cannot_return_success(self):
        for option in ("close_error", "pipe_error", "group_error"):
            failure = OSError("SECRET cleanup")
            result, error, output, _ = self.linux_fixture_simulation(
                [(1, "stdout", b"{}"), (1.1, "stdout", b""), (1.1, "stderr", b"")],
                exit_at=1,
                **{option: failure},
            )
            self.assertIsNone(result)
            self.assertIs(error, failure)
            self.assertFalse(error.ownership_fixture_cleanup["cleanup_complete"])
            self.assertTrue(error.ownership_fixture_cleanup["retain_workload_lease"])
            self.assertNotIn("SECRET", output)

    def test_linux_fixture_valid_multibyte_stderr_remains_complete_for_check(self):
        stderr = "€" * 3000
        for code in (0, 37):
            with patch.object(locale, "getencoding", return_value="utf-8"):
                result, error, output, _ = self.linux_fixture_simulation(
                    [
                        (1, "stdout", b'{"ok":true}'),
                        (1, "stderr", stderr.encode("utf-8")),
                        (1.1, "stdout", b""),
                        (1.1, "stderr", b""),
                    ],
                    exit_at=1,
                    code=code,
                )
            observed = result if code == 0 else error
            self.assertEqual(observed.stderr, stderr)
            self.assertEqual(observed.returncode, code)
            if code:
                self.assertIsInstance(error, subprocess.CalledProcessError)
            self.assertEqual(output, "")

    def test_linux_fixture_invalid_encoding_remains_strict(self):
        with patch.object(locale, "getencoding", return_value="utf-8"):
            _, error, output, _ = self.linux_fixture_simulation(
                [
                    (1, "stdout", b'{"ok":true}'),
                    (1, "stderr", b"\xff"),
                    (1.1, "stdout", b""),
                    (1.1, "stderr", b""),
                ],
                exit_at=1,
            )
        self.assertIsInstance(error, UnicodeDecodeError)
        self.assertEqual(output, "")

    def test_linux_fixture_capture_bounds_and_long_line_cannot_forge_marker(self):
        capture = OwnershipFixtureCapture()
        capture.feed("stderr", b"x" * 513, 1)
        capture.feed("stderr", b"OWNERSHIP_FIXTURE_ENTRY\n", 2)
        capture.feed("stderr", b"OWNERSHIP_FIXTURE_ENTRY\n", 3)
        self.assertEqual(capture.entry_ms, 3)
        capture.feed("stderr", b"SECRET\n" * 2000, 4)
        self.assertLessEqual(len(capture.stderr_tail), 8192)
        self.assertGreater(len(capture.stderr), 8192)
        with self.assertRaises(OverflowError):
            capture.feed("stdout", b"x" * (m.MAX_SAMPLE + 1), 5)
        with self.assertRaises(OverflowError):
            capture.feed("stderr", b"x" * (1024 * 1024 + 1), 6)

    def test_linux_fixture_cleanup_escalates_only_owned_group_and_rejects_reuse(self):
        for reused in (False, True):
            clock, alive = [0.0], [True]
            child = Mock(pid=12345, returncode=None)

            def poll():
                if not alive[0]:
                    child.returncode = -9

            def kill(group, sig):
                self.assertEqual(group, 12345)
                if not alive[0]:
                    raise ProcessLookupError
                if sig == signal.SIGKILL:
                    alive[0] = False

            child.poll.side_effect = poll
            with (
                patch.object(signal, "SIGKILL", 9, create=True),
                patch.object(os, "getpgrp", return_value=111, create=True),
                patch.object(os, "killpg", side_effect=kill, create=True) as signals,
                patch(
                    __name__ + ".ownership_fixture_birth",
                    return_value=43 if reused else 42,
                ),
                patch.object(time, "monotonic", side_effect=lambda: clock[0]),
                patch.object(
                    time,
                    "sleep",
                    side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay),
                ),
            ):
                if reused:
                    with self.assertRaises(RuntimeError):
                        cleanup_ownership_fixture(child, 42)
                    self.assertTrue(
                        all(call.args[1] == 0 for call in signals.call_args_list)
                    )
                else:
                    cleanup_ownership_fixture(child, 42)
                    self.assertEqual(
                        [
                            call.args[1]
                            for call in signals.call_args_list
                            if call.args[1]
                        ],
                        [signal.SIGTERM, signal.SIGKILL],
                    )
                    self.assertLessEqual(clock[0], 3.01)

    def test_ownership_timeout_summary_retains_only_known_stage_and_numbers(self):
        summary = ownership_fixture_timeout_summary(
            b"SECRET endpoint/path\n"
            b"OWNERSHIP_FIXTURE stage=entered elapsed_ms=0 pwsh_major=7 pwsh_minor=4\n"
            b"OWNERSHIP_FIXTURE stage=collector-call elapsed_ms=120 pwsh_major=7 pwsh_minor=4\n"
            b"SECRET token\n"
        )
        data = json.loads(summary.split(" ", 1)[1])
        self.assertEqual(data["stages"], ["entered", "collector-call"])
        self.assertEqual(data["elapsed_ms"], 120)
        self.assertEqual(data["pwsh_version"], [7, 4])
        self.assertNotIn("SECRET", summary)

    def test_ownership_timeout_summary_missing_or_unrecognized_is_unknown(self):
        for value in (
            None,
            "SECRET",
            b"SECRET",
            "OWNERSHIP_FIXTURE stage=secret-path elapsed_ms=1 pwsh_major=7 pwsh_minor=4",
            "OWNERSHIP_FIXTURE stage=entered elapsed_ms=99999 pwsh_major=7 pwsh_minor=4",
            "OWNERSHIP_FIXTURE stage=entered elapsed_ms=1 pwsh_major=999 pwsh_minor=4",
        ):
            with self.subTest(value=type(value).__name__):
                self.assertEqual(
                    {
                        key: result
                        for key, result in json.loads(
                            ownership_fixture_timeout_summary(value).split(" ", 1)[1]
                        ).items()
                        if key in {"stages", "elapsed_ms", "pwsh_version"}
                    },
                    {"stages": [], "elapsed_ms": None, "pwsh_version": None},
                )

    def test_ownership_timeout_capture_metadata_distinguishes_absence_and_empty(self):
        for value, present, kind, count in (
            (None, False, "none", None),
            (b"", True, "bytes", 0),
            ("", True, "text", 0),
            (b"SECRET", True, "bytes", 6),
            ("é", True, "text", 2),
            (b"\xff", True, "bytes", 1),
            (42, True, "unsupported", None),
        ):
            summary = ownership_fixture_timeout_summary(value, value)
            data = json.loads(summary.split(" ", 1)[1])
            for key in ("stdout_capture", "stderr_capture"):
                self.assertEqual(
                    data[key],
                    {
                        "present": present,
                        "kind": kind,
                        "byte_count": count,
                        "byte_count_clamped": False,
                    },
                )
            self.assertFalse(data["literal_entry_seen_in_retained_stderr"])
            self.assertEqual(data["stages"], [])
            self.assertNotIn("SECRET", summary)

    def test_ownership_timeout_literal_and_capture_windows_are_bounded(self):
        literal = "OWNERSHIP_FIXTURE_ENTRY"
        for value in (literal, literal.encode()):
            data = json.loads(ownership_fixture_timeout_summary(value).split(" ", 1)[1])
            self.assertTrue(data["literal_entry_seen_in_retained_stderr"])
            self.assertEqual(data["stages"], [])
            self.assertIsNone(data["elapsed_ms"])
        for value in (
            "prefix" + literal,
            literal + " suffix",
            literal + "\n" + "\n".join(["SECRET"] * 65),
            literal + "\n" + "é" * 9000,
            literal.encode() + b"\n" + b"SECRET" * 2000,
        ):
            summary = ownership_fixture_timeout_summary(value, value)
            data = json.loads(summary.split(" ", 1)[1])
            self.assertFalse(data["literal_entry_seen_in_retained_stderr"])
            self.assertEqual(data["stderr_window_limit"], 8192)
            self.assertEqual(data["stderr_line_limit"], 64)
            self.assertNotIn("SECRET", summary)
            if len(value) > 8192:
                for key in ("stdout_capture", "stderr_capture"):
                    self.assertEqual(data[key]["byte_count"], 8192)
                    self.assertTrue(data[key]["byte_count_clamped"])

    def test_ownership_timeout_diagnostics_never_turn_timeout_into_success(self):
        for stderr in (
            b"SECRET\nOWNERSHIP_FIXTURE stage=entered elapsed_ms=0 pwsh_major=7 pwsh_minor=4\n",
            b"SECRET unrecognized failure",
        ):
            error = subprocess.TimeoutExpired(
                ["SECRET"], 10, output=b"SECRET stdout", stderr=stderr
            )
            output = io.StringIO()
            with (
                patch.object(subprocess, "run", side_effect=error),
                patch(__name__ + ".run_linux_ownership_fixture", side_effect=error),
                contextlib.redirect_stderr(output),
                self.assertRaises(subprocess.TimeoutExpired) as raised,
            ):
                self.run_ownership_fixture()
            self.assertIs(raised.exception, error)
            self.assertIn("ownership-fixture-timeout ", output.getvalue())
            self.assertNotIn("SECRET", output.getvalue())

    def test_actual_fixture_caller_preserves_timeout_through_broken_diagnostic(self):
        class BrokenStderr(io.StringIO):
            def write(self, value):
                raise OSError("SECRET stderr unavailable")

        for platform in ("linux", "win32"):
            for mode in ("stderr", "summary"):
                error = subprocess.TimeoutExpired(["SECRET"], 10)
                error.ownership_fixture_cleanup = {
                    "cleanup_complete": False,
                    "failed_phases": ["group_cleanup"],
                    "retain_workload_lease": True,
                }
                try:
                    raise error
                except subprocess.TimeoutExpired:
                    original_traceback = error.__traceback__
                caught = None
                with (
                    patch.object(sys, "platform", platform),
                    patch(
                        __name__ + ".run_linux_ownership_fixture", side_effect=error
                    ) as linux,
                    patch.object(subprocess, "run", side_effect=error) as windows,
                    patch(
                        __name__ + ".ownership_fixture_timeout_summary",
                        side_effect=ValueError("SECRET summary")
                        if mode == "summary"
                        else None,
                        wraps=ownership_fixture_timeout_summary,
                    ),
                    contextlib.redirect_stderr(BrokenStderr()),
                ):
                    try:
                        self.run_ownership_fixture()
                    except BaseException as failure:
                        caught = failure
                self.assertIs(caught, error)
                facts = error.ownership_fixture_cleanup
                self.assertEqual(
                    facts["failed_phases"], ["group_cleanup", "caller_diagnostic_write"]
                )
                self.assertFalse(facts["cleanup_complete"])
                self.assertTrue(facts["retain_workload_lease"])
                self.assertNotIn("SECRET", json.dumps(facts))
                tb = error.__traceback__
                while tb is not None and tb is not original_traceback:
                    tb = tb.tb_next
                self.assertIs(tb, original_traceback)
                if platform == "linux":
                    linux.assert_called_once()
                    windows.assert_not_called()
                else:
                    windows.assert_called_once()
                    linux.assert_not_called()
                    self.assertEqual(
                        windows.call_args.kwargs,
                        {
                            "capture_output": True,
                            "text": True,
                            "timeout": 10,
                            "check": True,
                        },
                    )

    def test_actual_fixture_caller_preserves_primary_through_temp_cleanup_failure(self):
        for platform in ("linux", "win32"):
            error = subprocess.TimeoutExpired(["fixture"], 10)
            with tempfile.TemporaryDirectory() as temp:
                temporary = Mock(name=temp)
                temporary.name = temp
                temporary.cleanup.side_effect = OSError("SECRET temp cleanup")
                with (
                    patch.object(sys, "platform", platform),
                    patch(__name__ + ".run_linux_ownership_fixture", side_effect=error),
                    patch.object(subprocess, "run", side_effect=error),
                    patch.object(
                        tempfile, "TemporaryDirectory", return_value=temporary
                    ),
                    contextlib.redirect_stderr(io.StringIO()) as output,
                    self.assertRaises(subprocess.TimeoutExpired) as raised,
                ):
                    self.run_ownership_fixture()
            self.assertIs(raised.exception, error)
            self.assertEqual(
                error.ownership_fixture_cleanup["failed_phases"],
                ["fixture_temp_cleanup"],
            )
            self.assertTrue(error.ownership_fixture_cleanup["retain_workload_lease"])
            self.assertNotIn("SECRET", output.getvalue())

    def test_actual_fixture_temp_cleanup_failure_without_primary_stays_failure(self):
        failure = OSError("SECRET temp cleanup")
        with tempfile.TemporaryDirectory() as temp:
            temporary = Mock()
            temporary.name = temp
            temporary.cleanup.side_effect = failure
            with (
                patch.object(sys, "platform", "linux"),
                patch(
                    __name__ + ".run_linux_ownership_fixture",
                    return_value=subprocess.CompletedProcess(["fixture"], 0, "{}", ""),
                ),
                patch.object(tempfile, "TemporaryDirectory", return_value=temporary),
                self.assertRaises(OSError) as raised,
            ):
                self.run_ownership_fixture()
        self.assertIs(raised.exception, failure)

    def test_scope_requires_every_gate_and_actual_windows(self):
        env = dict(
            GITHUB_REPOSITORY="CompleteDotTech/codex-subagents-multi-provider",
            GITHUB_ACTIONS="true",
            RUNNER_OS="Windows",
            RUNNER_ENVIRONMENT="github-hosted",
        )
        self.assertTrue(m.authorized(env, "Windows"))
        for key in env:
            altered = dict(env, **{key: "other"})
            self.assertFalse(m.authorized(altered, "Windows"))
        self.assertFalse(m.authorized(env, "Linux"))

    def test_timeout_and_errors_do_not_leak(self):
        for error in (
            OSError("SECRET"),
            subprocess.TimeoutExpired("SECRET", 10),
            ValueError("SECRET"),
        ):
            with patch.object(m, "bounded_output", side_effect=error):
                result = m.collect(123, 1)
                self.assertEqual(result["unavailable"], 1)
                self.assertNotIn("SECRET", json.dumps(result))

    def test_collection_deadline_and_no_shell(self):
        with patch.object(m, "bounded_output", return_value=b"{}") as run:
            m.collect(123, 4)
            self.assertEqual(run.call_args.kwargs["timeout"], 10)
            self.assertNotIn("shell", run.call_args.kwargs)
            self.assertEqual(
                run.call_args.args[0][-4:],
                ["-OwnedPid", "123", "-CreationMicroseconds", "4"],
            )

    def test_build_exit_preserved_with_observer_failure(self):
        observer = Mock()
        observer.sample.side_effect = OSError("SECRET")
        child = Mock()
        child.wait.side_effect = [subprocess.TimeoutExpired("fixture", 60), 37]
        child.pid = 123
        with (
            patch.object(m.subprocess, "Popen", return_value=child) as launch,
            contextlib.redirect_stdout(io.StringIO()) as out,
        ):
            self.assertEqual(
                m.supervise(["bash", "file with spaces", "--jobs=8"], observer), 37
            )
        self.assertNotIn("SECRET", out.getvalue())
        launch.assert_called_once_with(["bash", "file with spaces", "--jobs=8"])
        child.kill.assert_not_called()
        self.assertEqual(observer.sample.call_args_list[-1].args[0], "build-exit")

    def test_start_failure_is_real_and_not_success(self):
        with patch.object(
            m.subprocess, "Popen", side_effect=FileNotFoundError("fixture")
        ):
            with self.assertRaises(FileNotFoundError):
                m.supervise(["missing-build"], None)

    def workflow_git_bash_probe(self, setup):
        workflow = (Path(__file__).parents[1] / "workflows/bazel.yml").read_text()
        step = workflow.split("      - name: Diagnose native MSVC voice runtime\n", 1)[
            1
        ].split("      - name:", 1)[0]
        script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        launch = re.search(
            r'python \.github/scripts/observe_windows_ci_resources\.py supervise -- ("[^"\n]+"|[^\s]+)',
            script,
        )
        self.assertIsNotNone(launch)
        # Execute the workflow's real shell selection, without its build payload.
        probe = setup + script[: launch.start()] + "printf '%s\\0' " + launch.group(1)
        outer = Path(os.environ["ProgramFiles"]) / "Git/bin/bash.exe"
        self.assertTrue(outer.is_file(), "Git Bash is required on Windows")
        with tempfile.TemporaryDirectory() as directory:
            script_path = Path(directory) / "workflow selection.sh"
            script_path.write_text(probe, newline="\n")
            result = subprocess.run(
                [
                    str(outer),
                    "--noprofile",
                    "--norc",
                    "-e",
                    "-o",
                    "pipefail",
                    script_path.as_posix(),
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=dict(os.environ, ISSUE67_NON_SHELL=sys.executable),
                timeout=10,
                check=False,
            )
        line, separator, selected = result.stdout.partition(b"\n")
        self.assertEqual(separator, b"\n")
        prefix = b"git-bash-selection "
        self.assertTrue(line.startswith(prefix))
        diagnostics = json.loads(line[len(prefix) :])
        self.assertEqual(
            set(diagnostics),
            {
                "original_ostype_msys",
                "original_bash_posix_absolute",
                "original_bash_executable",
                "path_form",
            },
        )
        for key in diagnostics.keys() - {"path_form"}:
            self.assertIs(type(diagnostics[key]), bool)
        self.assertIn(
            diagnostics["path_form"],
            ("relative", "posix_absolute", "windows_absolute"),
        )
        return result, diagnostics, selected

    @unittest.skipUnless(sys.platform == "win32", "native Windows command selection")
    def test_workflow_selects_current_git_bash_and_preserves_native_child_exit(self):
        executables = []
        for setup, form in (
            ('BASH="$(cygpath -au "$BASH")"\n', "posix_absolute"),
            (
                'BASH="$(cygpath -aw "$BASH")"\nOSTYPE=fixture\n',
                "windows_absolute",
            ),
        ):
            with self.subTest(form=form):
                result, diagnostics, selected = self.workflow_git_bash_probe(setup)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(diagnostics["path_form"], form)
                if form == "windows_absolute":
                    self.assertFalse(diagnostics["original_ostype_msys"])
                    self.assertFalse(diagnostics["original_bash_posix_absolute"])
                self.assertEqual(selected.count(b"\0"), 1)
                executable = Path(selected[:-1].decode())
                self.assertTrue(
                    executable.is_absolute(), "never resolve a bare bash through PATH"
                )
                self.assertTrue(executable.is_file())
                executables.append(executable)
                observer = Mock()
                with tempfile.TemporaryDirectory() as directory:
                    marker = Path(directory) / "shell identity.txt"
                    child_script = Path(directory) / "harmless child.sh"
                    child_script.write_text(
                        'printf "%s\\n" "$BASH_VERSION" "$2" > "$1"\nexit 37\n',
                        newline="\n",
                    )
                    with patch.object(
                        m, "collect", side_effect=AssertionError("no host CIM")
                    ):
                        code = m.supervise(
                            [
                                str(executable),
                                "--noprofile",
                                "--norc",
                                child_script.as_posix(),
                                marker.as_posix(),
                                "argument with spaces",
                            ],
                            observer,
                        )
                    self.assertEqual(code, 37)
                    version, argument = marker.read_text().splitlines()
                    self.assertTrue(re.fullmatch(r"\d+\.\d+.*", version))
                    self.assertEqual(argument, "argument with spaces")
                self.assertEqual(
                    [call.args[0] for call in observer.sample.call_args_list],
                    ["before-build", "build-exit"],
                )
        self.assertTrue(executables[0].samefile(executables[1]))

    @unittest.skipUnless(sys.platform == "win32", "native Windows command selection")
    def test_workflow_rejects_relative_missing_and_non_shell_executables(self):
        for setup in (
            "BASH=bash\n",
            'BASH="$(cygpath -aw "$BASH").does-not-exist"\n',
            'BASH="$(cygpath -aw "$ISSUE67_NON_SHELL")"\n',
        ):
            with self.subTest(setup=setup):
                result, diagnostics, selected = self.workflow_git_bash_probe(setup)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(selected, b"")
                self.assertNotIn(sys.executable.encode(), result.stdout + result.stderr)

    def test_oversized_collector_rejected(self):
        with patch.object(m, "bounded_output", side_effect=OverflowError()):
            self.assertEqual(m.collect(), {"unavailable": 1, "truncated": True})

    def test_real_collector_overflow_is_bounded_and_reader_reaped(self):
        started = time.monotonic()
        with self.assertRaises(OverflowError):
            m.bounded_output(
                [
                    sys.executable,
                    "-c",
                    "import os,time; os.write(1,b'x'*1000000); time.sleep(30)",
                ],
                timeout=2,
            )
        self.assertLess(time.monotonic() - started, 3)
        self.assertFalse(
            any(
                t.name == "windows-resource-collector-reader"
                for t in threading.enumerate()
            )
        )

    def test_real_collector_timeout_and_stderr_are_bounded(self):
        started = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            m.bounded_output(
                [
                    sys.executable,
                    "-c",
                    "import os,time; os.write(2,b'SECRET'*100000); time.sleep(30)",
                ],
                timeout=0.2,
            )
        self.assertLess(time.monotonic() - started, 2)
        self.assertFalse(
            any(
                t.name == "windows-resource-collector-reader"
                for t in threading.enumerate()
            )
        )

    def test_real_collector_success_and_nonzero_cleanup(self):
        self.assertEqual(
            m.bounded_output([sys.executable, "-c", "print('{}')"]),
            b"{}\r\n" if sys.platform == "win32" else b"{}\n",
        )
        with self.assertRaises(ValueError):
            m.bounded_output([sys.executable, "-c", "raise SystemExit(4)"])
        self.assertFalse(
            any(
                t.name == "windows-resource-collector-reader"
                for t in threading.enumerate()
            )
        )

    def test_owned_process_handle_identity_has_microsecond_precision(self):
        child = Mock(_handle=42)

        def api(handle, creation, exit_time, kernel, user):
            self.assertEqual(handle, 42)
            creation._obj.value = 123456789012345678
            return 1

        self.assertEqual(m.owned_creation_microseconds(child, api), 12345678901234567)
        self.assertIsNone(m.owned_creation_microseconds(child, lambda *args: 0))

    def test_schema_rejects_secrets_nonfinite_and_nonallowlisted_images(self):
        data = m.sanitize(
            dict(
                memory={"available_bytes": float("inf"), "SECRET": 1},
                volumes=[dict(volume="SECRET", free_bytes="SECRET", total_bytes=12)],
                pagefiles=[dict(volume="C:\\", allocated_bytes=1048576)],
                processes=[dict(image="SECRET", private_bytes=1)],
                SECRET="SECRET",
                vhd=dict(backing_volume="C:\\", logical_size_bytes=68719476736),
            )
        )
        self.assertNotIn("SECRET", json.dumps(data))
        self.assertIsNone(data["memory"]["available_bytes"])
        self.assertIsNone(data["volumes"][0]["volume"])
        self.assertEqual(data["pagefiles"][0]["volume"], "C:")
        self.assertEqual(data["processes"], [])
        self.assertEqual(data["vhd"]["logical_size_bytes"], 68719476736)
        self.assertIsNone(data["vhd"]["allocated_physical_bytes"])

    def test_caps_and_stdout_precedes_write(self):
        with (
            tempfile.TemporaryDirectory() as temp,
            patch.dict(m.os.environ, {"RUNNER_TEMP": temp}),
            patch.object(m, "identity", return_value={"head": "fixture"}),
            patch.object(
                m,
                "collect",
                return_value={"memory": {"available_bytes": 8}, "unavailable": 0},
            ),
        ):
            observer = m.Observer()
            with contextlib.redirect_stdout(io.StringIO()) as out:
                observer.sample("before-prepare")
            self.assertIn("available_bytes", out.getvalue())
            self.assertLess(observer.path.stat().st_size, m.MAX_SAMPLE)
            observer.count = m.MAX_SAMPLES
            with contextlib.redirect_stdout(io.StringIO()):
                observer.sample("after-prepare")
            self.assertEqual(len(observer.path.read_text().splitlines()), 1)
            observer.count = 0
            observer.size = m.MAX_TOTAL
            with contextlib.redirect_stdout(io.StringIO()):
                observer.sample("after-prepare")
            self.assertEqual(len(observer.path.read_text().splitlines()), 1)

    def test_workflow_only_diagnostic_job_uses_observer(self):
        workflow = Path(__file__).parents[1] / "workflows" / "bazel.yml"
        before, job = workflow.read_text().split("  voice-runtime-diagnostic:", 1)
        self.assertNotIn("observe_windows_ci_resources", before)
        self.assertIn(
            'python .github/scripts/observe_windows_ci_resources.py supervise -- "$git_bash" ./.github/scripts/run-bazel-ci.sh',
            job,
        )
        self.assertIn(
            "if: github.event_name == 'workflow_dispatch' && inputs.voice_runtime_diagnostic",
            job,
        )
        self.assertEqual(job.count("if: always() && !cancelled()"), 4)
        self.assertNotIn("--jobs", job)
        for step in job.split("      - name: ")[1:]:
            if step.startswith("Observe resources "):
                self.assertIn("continue-on-error: true", step)

    def test_nested_observation_input_and_order(self):
        actions = Path(__file__).parents[1] / "actions"
        prepare = (actions / "prepare-bazel-ci" / "action.yml").read_text()
        setup = (actions / "setup-bazel-ci" / "action.yml").read_text()
        for text in (prepare, setup):
            self.assertIn("windows-resource-observation:", text)
            self.assertIn(
                'default: "false"', text.split("windows-resource-observation:", 1)[1]
            )
        self.assertIn(
            "windows-resource-observation: ${{ inputs.windows-resource-observation }}",
            prepare,
        )
        self.assertLess(setup.index("id: setup_ci"), setup.index("after-provision"))
        self.assertLess(
            setup.index("after-provision"),
            setup.index("Provision pinned native Windows"),
        )
        workflow = (Path(__file__).parents[1] / "workflows" / "bazel.yml").read_text()
        self.assertEqual(workflow.count("windows-resource-observation: true"), 1)
        self.assertNotIn(
            "windows-resource-observation",
            workflow.split("  voice-runtime-diagnostic:")[0],
        )

    def run_ownership_fixture(
        self,
        offset=0,
        children=2,
        missing_identity=False,
        reuse_during_query=False,
        stale_nonroot=False,
    ):
        script = Path(__file__).with_name("observe_windows_ci_resources.ps1").resolve()
        fixture = r"""
[Console]::Error.WriteLine('OWNERSHIP_FIXTURE_ENTRY')
$global:ownershipFixtureClock = [Diagnostics.Stopwatch]::StartNew()
$global:ownershipFixtureStageCount = 0
function global:Write-OwnershipFixtureStage {
    param([string]$Stage)
    if ($global:ownershipFixtureStageCount -ge 64 -and $Stage -ne 'collector-return') { return }
    $global:ownershipFixtureStageCount++
    $elapsed = [Math]::Min(60000, $global:ownershipFixtureClock.ElapsedMilliseconds)
    [Console]::Error.WriteLine("OWNERSHIP_FIXTURE stage=$Stage elapsed_ms=$elapsed pwsh_major=$($PSVersionTable.PSVersion.Major) pwsh_minor=$($PSVersionTable.PSVersion.Minor)")
}
Write-OwnershipFixtureStage 'entered'
$global:fixtureQueries = 0
$global:fixtureCreated = [datetime]::new(2026, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
function global:Get-CimInstance {
    param($ClassName, $Filter, $OperationTimeoutSec, $ErrorAction)
    switch ($ClassName) {
        'Win32_PerfFormattedData_PerfOS_Memory' { Write-OwnershipFixtureStage 'query-memory' }
        'Win32_PageFileUsage' { Write-OwnershipFixtureStage 'query-pagefiles' }
        'Win32_PageFileSetting' { Write-OwnershipFixtureStage 'query-settings' }
        'Win32_LogicalDisk' { Write-OwnershipFixtureStage 'query-volume' }
        'Win32_Process' { Write-OwnershipFixtureStage 'query-process' }
    }
    switch ($ClassName) {
        'Win32_PerfFormattedData_PerfOS_Memory' { [pscustomobject]@{ AvailableBytes=$null; CommittedBytes=200; CommitLimit=400; PercentCommittedBytesInUse=50 } }
        'Win32_PageFileUsage' { [pscustomobject]@{ Name='C:\pagefile.sys'; AllocatedBaseSize=$null; CurrentUsage=1; PeakUsage=2 } }
        'Win32_PageFileSetting' { [pscustomobject]@{ Name='C:\pagefile.sys'; InitialSize=2; MaximumSize=4 } }
        'Win32_LogicalDisk' { [pscustomobject]@{ FreeSpace=1024; Size=8192 } }
        'Win32_Process' {
            if ($Filter -eq 'ProcessId=101') { $global:fixtureQueries++; $date=$global:fixtureCreated; if (REUSE -and $global:fixtureQueries -ge 3) { $date=$date.AddTicks(10) }; [pscustomobject]@{ ProcessId=101; CreationDate=$date }; return }
            if (STALEMODE) {
                if ($Filter -eq 'ParentProcessId=101') { [pscustomobject]@{ ProcessId=201; CreationDate=$global:fixtureCreated.AddMilliseconds(2); Name='cmake.exe'; WorkingSetSize=200; PrivatePageCount=200 }; return }
                if ($Filter -eq 'ProcessId=201') { [pscustomobject]@{ CreationDate=$global:fixtureCreated.AddMilliseconds(2) }; return }
                if ($Filter -eq 'ParentProcessId=201') {
                    [pscustomobject]@{ ProcessId=301; CreationDate=$global:fixtureCreated.AddMilliseconds(1); Name='rustc.exe'; WorkingSetSize=100; PrivatePageCount=100 }
                    [pscustomobject]@{ ProcessId=302; CreationDate=$global:fixtureCreated.AddMilliseconds(3); Name='link.exe'; WorkingSetSize=300; PrivatePageCount=300 }
                    return
                }
                if ($Filter -eq 'ProcessId=302') { [pscustomobject]@{ CreationDate=$global:fixtureCreated.AddMilliseconds(3) }; return }
            }
            if ($Filter -eq 'ParentProcessId=101') {
                1..CHILDREN | ForEach-Object { [pscustomobject]@{ ProcessId=(200+$_); CreationDate=$global:fixtureCreated.AddMilliseconds(1); Name='rustc.exe'; WorkingSetSize=100; PrivatePageCount=$null } }; return
            }
            if ($Filter -match '^ProcessId=\d+$') { [pscustomobject]@{ CreationDate=$global:fixtureCreated.AddMilliseconds(1) }; return }
            if ($Filter -match '^ParentProcessId=\d+$') { return }
            throw 'unscoped host query prohibited'
        }
        default { throw 'host query prohibited' }
    }
}
function global:Test-Path {
    param($LiteralPath, $PathType)
    Write-OwnershipFixtureStage 'vhd-test'
    if ($LiteralPath -ne $global:fixtureVhd) { throw 'unexpected VHD path' }
    return $true
}
function global:Get-Item {
    param($LiteralPath)
    Write-OwnershipFixtureStage 'vhd-item'
    if ($LiteralPath -ne $global:fixtureVhd) { throw 'unexpected VHD path' }
    [pscustomobject]@{ Length=4096 }
}
$env:RUNNER_TEMP = 'FIXTURE_ROOT'
$global:fixtureVhd = Join-Path $env:RUNNER_TEMP 'codex-dev-drive.vhdx'
$epoch = [datetime]::new(1601, 1, 1, 0, 0, 0, [DateTimeKind]::Utc).Ticks
$micro = [long](($global:fixtureCreated.Ticks - $epoch) / 10D) + OFFSET
Write-OwnershipFixtureStage 'mocks-ready'
Write-OwnershipFixtureStage 'collector-call'
& $args[0] -OwnedPid 101 -CreationMicroseconds $micro
Write-OwnershipFixtureStage 'collector-return'
"""
        # Keep Windows C: semantics there; Linux PowerShell needs a native
        # filesystem root for Join-Path and System.IO.Path root interpretation.
        fixture_root = "C:\\fixture" if os.name == "nt" else tempfile.gettempdir()
        fixture = fixture.replace("FIXTURE_ROOT", fixture_root.replace("'", "''"))
        fixture = fixture.replace("CHILDREN", str(children)).replace(
            "OFFSET", str(offset)
        )
        fixture = fixture.replace("REUSE", "$true" if reuse_during_query else "$false")
        fixture = fixture.replace("STALEMODE", "$true" if stale_nonroot else "$false")
        if missing_identity:
            fixture = fixture.replace(
                "-CreationMicroseconds $micro", "-CreationMicroseconds 0"
            )
        temporary = tempfile.TemporaryDirectory()
        primary = None
        primary_traceback = None
        try:
            runner = Path(temporary.name) / "fixture.ps1"
            runner.write_text(fixture)
            try:
                argv = ["pwsh", "-NoProfile", "-File", str(runner), str(script)]
                if sys.platform == "linux":
                    result = run_linux_ownership_fixture(argv)
                else:
                    result = subprocess.run(
                        argv, capture_output=True, text=True, timeout=10, check=True
                    )
            except subprocess.TimeoutExpired as error:
                try:
                    print(
                        ownership_fixture_timeout_summary(error.stderr, error.stdout),
                        file=sys.stderr,
                    )
                except BaseException:
                    record_ownership_fixture_secondary_failure(
                        error, "caller_diagnostic_write"
                    )
                raise
        except BaseException as error:
            primary, primary_traceback = error, error.__traceback__
        finally:
            try:
                temporary.cleanup()
            except BaseException as error:
                if primary is None:
                    record_ownership_fixture_secondary_failure(
                        error, "fixture_temp_cleanup"
                    )
                    raise
                record_ownership_fixture_secondary_failure(
                    primary, "fixture_temp_cleanup"
                )
        if primary is not None:
            raise primary.with_traceback(primary_traceback)
        return json.loads(result.stdout)

    def test_powershell_exact_root_identity_and_numeric_vhd_fixture(self):
        data = self.run_ownership_fixture()
        self.assertEqual(len(data["processes"]), 2)
        self.assertEqual(data["processes"][0]["working_bytes"], 100)
        self.assertIsNone(data["processes"][0]["private_bytes"])
        self.assertIsNone(data["memory"])
        self.assertIsNone(data["pagefiles"][0]["allocated_bytes"])
        self.assertEqual(
            data["configured_pagefiles"][0]["maximum_bytes"], 4 * 1024 * 1024
        )
        self.assertEqual(data["vhd"]["file_length_bytes"], 4096)
        self.assertEqual(data["vhd"]["logical_size_bytes"], 68719476736)
        self.assertEqual(
            data["vhd"]["backing_volume"],
            "C:\\" if os.name == "nt" else Path(tempfile.gettempdir()).anchor,
        )
        self.assertIsNone(data["vhd"]["allocated_physical_bytes"])
        self.assertLessEqual(len(data["volumes"]), 16)

    def test_powershell_nearby_reused_root_and_missing_handle_rejected(self):
        for options in ({"offset": 1000000}, {"offset": 1}, {"missing_identity": True}):
            data = self.run_ownership_fixture(**options)
            self.assertEqual(data["processes"], [])
            self.assertGreater(data["unavailable"], 0)

    def test_powershell_parent_pid_reuse_during_query_rejected(self):
        data = self.run_ownership_fixture(reuse_during_query=True)
        self.assertEqual(data["processes"], [])
        self.assertGreater(data["unavailable"], 0)

    def test_artifact_failure_does_not_remove_prior_numeric_console(self):
        with (
            tempfile.TemporaryDirectory() as temp,
            patch.dict(m.os.environ, {"RUNNER_TEMP": temp}),
            patch.object(m, "identity", return_value={}),
            patch.object(
                m, "collect", return_value={"memory": {"available_bytes": 123}}
            ),
        ):
            observer = m.Observer()
            output = io.StringIO()

            def fail_write(*args, **kwargs):
                self.assertIn('"available_bytes":123', output.getvalue())
                raise OSError("SECRET disk error")

            with (
                contextlib.redirect_stdout(output),
                patch.object(Path, "mkdir", side_effect=fail_write),
            ):
                observer.sample("after-prepare")
            self.assertIn("artifact_unavailable=1", output.getvalue())
            self.assertNotIn("SECRET", output.getvalue())

    def test_powershell_nonroot_parent_rejects_older_stale_child(self):
        data = self.run_ownership_fixture(stale_nonroot=True)
        self.assertEqual(
            [row["image"] for row in data["processes"]], ["cmake.exe", "link.exe"]
        )
        self.assertEqual(
            [row["working_bytes"] for row in data["processes"]], [200, 300]
        )
        self.assertGreater(data["unavailable"], 0)

    def test_powershell_owned_descendants_capped(self):
        data = self.run_ownership_fixture(children=200)
        self.assertTrue(data["truncated"])
        self.assertLessEqual(len(data["processes"]), 128)
        self.assertEqual(len(data["processes"]), 127)
        self.assertLess(len(json.dumps(data).encode()), m.MAX_SAMPLE)

    def test_powershell_numeric_fixture_without_host_queries(self):
        script = Path(__file__).with_name("observe_windows_ci_resources.ps1").resolve()
        fixture = r"""
function global:Get-CimInstance {
    param($ClassName, $Filter, $OperationTimeoutSec, $ErrorAction)
    switch ($ClassName) {
        'Win32_PerfFormattedData_PerfOS_Memory' { [pscustomobject]@{ AvailableBytes=100; CommittedBytes=200; CommitLimit=400; PercentCommittedBytesInUse=50 } }
        'Win32_PageFileUsage' { 1..18 | ForEach-Object { [pscustomobject]@{ Name='C:\pagefile.sys'; AllocatedBaseSize=2; CurrentUsage=1; PeakUsage=2 } } }
        'Win32_LogicalDisk' { throw 'SECRET CIM failure' }
        default { throw 'host query prohibited' }
    }
}
$env:RUNNER_TEMP = ''
& $args[0]
"""
        with tempfile.TemporaryDirectory() as temp:
            runner = Path(temp) / "fixture.ps1"
            runner.write_text(fixture)
            result = subprocess.run(
                ["pwsh", "-NoProfile", "-File", str(runner), str(script)],
                capture_output=True,
                text=True,
                timeout=10,
                check=True,
            )
        data = json.loads(result.stdout)
        self.assertEqual(data["memory"]["committed_bytes"], 200)
        self.assertEqual(data["pagefiles"][0]["allocated_bytes"], 2 * 1024 * 1024)
        self.assertEqual(len(data["pagefiles"]), 16)
        self.assertTrue(data["truncated"])
        self.assertIsNone(data["volumes"][0]["free_bytes"])
        self.assertNotIn("SECRET", result.stdout)
        self.assertLess(len(result.stdout.encode()), m.MAX_SAMPLE)


if __name__ == "__main__":
    unittest.main()
