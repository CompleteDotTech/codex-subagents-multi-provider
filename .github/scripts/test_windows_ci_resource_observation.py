"""Fixture-only checks: no host CIM queries or actual builds."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import sys
import threading
import time
import unittest
from unittest.mock import patch, Mock

SPEC = importlib.util.spec_from_file_location(
    "observation", Path(__file__).with_name("observe_windows_ci_resources.py")
)
m = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(m)


class ObservationTests(unittest.TestCase):
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
            "python .github/scripts/observe_windows_ci_resources.py supervise -- bash ./.github/scripts/run-bazel-ci.sh",
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
        self, offset=0, children=2, missing_identity=False, reuse_during_query=False
    ):
        script = Path(__file__).with_name("observe_windows_ci_resources.ps1").resolve()
        fixture = r"""
$global:fixtureQueries = 0
$global:fixtureCreated = [datetime]::new(2026, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
function global:Get-CimInstance {
    param($ClassName, $Filter, $OperationTimeoutSec, $ErrorAction)
    switch ($ClassName) {
        'Win32_PerfFormattedData_PerfOS_Memory' { [pscustomobject]@{ AvailableBytes=$null; CommittedBytes=200; CommitLimit=400; PercentCommittedBytesInUse=50 } }
        'Win32_PageFileUsage' { [pscustomobject]@{ Name='C:\pagefile.sys'; AllocatedBaseSize=$null; CurrentUsage=1; PeakUsage=2 } }
        'Win32_PageFileSetting' { [pscustomobject]@{ Name='C:\pagefile.sys'; InitialSize=2; MaximumSize=4 } }
        'Win32_LogicalDisk' { [pscustomobject]@{ FreeSpace=1024; Size=8192 } }
        'Win32_Process' {
            if ($Filter -eq 'ProcessId=101') { $global:fixtureQueries++; $date=$global:fixtureCreated; if (REUSE -and $global:fixtureQueries -ge 3) { $date=$date.AddTicks(10) }; [pscustomobject]@{ ProcessId=101; CreationDate=$date }; return }
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
    if ($LiteralPath -ne 'C:\fixture\codex-dev-drive.vhdx') { throw 'unexpected VHD path' }
    return $true
}
function global:Get-Item {
    param($LiteralPath)
    if ($LiteralPath -ne 'C:\fixture\codex-dev-drive.vhdx') { throw 'unexpected VHD path' }
    [pscustomobject]@{ Length=4096 }
}
$env:RUNNER_TEMP = 'C:\fixture'
$epoch = [datetime]::new(1601, 1, 1, 0, 0, 0, [DateTimeKind]::Utc).Ticks
$micro = [long](($global:fixtureCreated.Ticks - $epoch) / 10D) + OFFSET
& $args[0] -OwnedPid 101 -CreationMicroseconds $micro
"""
        fixture = fixture.replace("CHILDREN", str(children)).replace(
            "OFFSET", str(offset)
        )
        fixture = fixture.replace("REUSE", "$true" if reuse_during_query else "$false")
        if missing_identity:
            fixture = fixture.replace(
                "-CreationMicroseconds $micro", "-CreationMicroseconds 0"
            )
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
        self.assertEqual(data["vhd"]["backing_volume"], "C:\\".replace("\\\\", "\\"))
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
