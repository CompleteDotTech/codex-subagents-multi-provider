"""Fresh-job first PowerShell launch control; full mode retains all original oracles."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from unittest.mock import patch

MINIMAL = """[Console]::Error.WriteLine('OWNERSHIP_FIXTURE_ENTRY')
[Console]::Error.WriteLine("OWNERSHIP_FIXTURE stage=entered elapsed_ms=0 pwsh_major=$([Math]::Min(100,$PSVersionTable.PSVersion.Major)) pwsh_minor=$([Math]::Min(100,$PSVersionTable.PSVersion.Minor))")
[Console]::Out.WriteLine('{}')
"""


def load_fixture():
    spec = importlib.util.spec_from_file_location(
        "first_launch_fixture",
        Path(__file__).with_name("test_windows_ci_resource_observation.py"),
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def mark_secondary(error, phase):
    # Fixed facts retained on the original object even when stderr is broken.
    previous = getattr(error, "ownership_startup_cleanup", {})
    phases = previous.get("failed_phases", [])
    phases = [item for item in phases if item in ("summary", "temporary_cleanup")]
    if phase not in phases:
        phases.append(phase)
    error.ownership_startup_cleanup = {
        "cleanup_complete": False,
        "retain_workload_lease": True,
        "failed_phases": phases,
    }


def binary_digest():
    # Called only AFTER the trial, never touching the executable beforehand.
    executable = shutil.which("pwsh")
    if executable is None:
        return None
    with open(executable, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def summary(module, mode, captures, error, digest):
    capture = captures[0] if len(captures) == 1 else None
    known = json.loads(
        module.ownership_fixture_timeout_summary(
            bytes(capture.stderr_tail) if capture else None
        ).split(" ", 1)[1]
    )
    cleanup = getattr(error, "ownership_fixture_cleanup", None)
    return {
        "mode": mode,
        "control_only": mode == "minimal",
        "qualified": error is None,
        "qualification_timeout_seconds": 10,
        "failure_only_observation_seconds": 20,
        "parent_observed_first_entry_ms": capture.entry_ms if capture else None,
        "parent_observed_first_stage_ms": capture.stages if capture else {},
        "pwsh_version": known["pwsh_version"],
        "pwsh_binary_sha256_after_trial": digest,
        "cleanup_complete": error is None
        or bool(cleanup and cleanup.get("cleanup_complete")),
        "exit_category": "zero"
        if error is None
        else "timeout"
        if isinstance(error, subprocess.TimeoutExpired)
        else "failure",
    }


def run(mode, module=None):
    if mode not in ("minimal", "full"):
        raise ValueError("unknown first-launch mode")
    module = module or load_fixture()
    captures = []
    base = module.OwnershipFixtureCapture

    class RetainedCapture(base):
        def __init__(self):
            super().__init__()
            captures.append(self)

    temporary = None
    primary = traceback = None
    try:
        with patch.object(module, "OwnershipFixtureCapture", RetainedCapture):
            if mode == "full":
                case = module.ObservationTests(
                    "test_powershell_exact_root_identity_and_numeric_vhd_fixture"
                )
                case.test_powershell_exact_root_identity_and_numeric_vhd_fixture()
            else:
                temporary = tempfile.TemporaryDirectory()
                runner = Path(temporary.name) / "fixture.ps1"
                runner.write_text(MINIMAL, encoding="utf-8")
                script = (
                    Path(module.__file__)
                    .with_name("observe_windows_ci_resources.ps1")
                    .resolve()
                )
                result = module.run_linux_ownership_fixture(
                    ["pwsh", "-NoProfile", "-File", str(runner), str(script)]
                )
                if json.loads(result.stdout) != {}:
                    raise AssertionError("minimal control output mismatch")
    except BaseException as error:
        primary, traceback = error, error.__traceback__
    finally:
        if temporary is not None:
            try:
                temporary.cleanup()
            except BaseException as error:
                if primary is None:
                    primary, traceback = error, error.__traceback__
                mark_secondary(primary, "temporary_cleanup")
        try:
            digest = binary_digest()
            print(
                "ownership-startup-control "
                + json.dumps(
                    summary(module, mode, captures, primary, digest),
                    separators=(",", ":"),
                ),
                file=sys.stderr,
            )
        except BaseException as error:
            if primary is None:
                primary, traceback = error, error.__traceback__
            mark_secondary(primary, "summary")
    if primary is not None:
        raise primary.with_traceback(traceback)


if __name__ == "__main__":
    try:
        run(os.environ.get("OWNERSHIP_STARTUP_MODE"))
    except BaseException:
        # Never render private exception/argv/path text in hosted logs.
        sys.exit(1)
