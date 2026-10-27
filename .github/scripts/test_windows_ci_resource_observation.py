"""Fixture-only checks: no host CIM queries or actual builds."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch, Mock

SPEC = importlib.util.spec_from_file_location('observation', Path(__file__).with_name('observe_windows_ci_resources.py'))
m = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(m)


class ObservationTests(unittest.TestCase):
    def test_scope_requires_every_gate_and_actual_windows(self):
        env = dict(GITHUB_REPOSITORY='CompleteDotTech/codex-subagents-multi-provider', GITHUB_ACTIONS='true', RUNNER_OS='Windows', RUNNER_ENVIRONMENT='github-hosted')
        self.assertTrue(m.authorized(env, 'Windows'))
        for key in env:
            altered = dict(env, **{key: 'other'})
            self.assertFalse(m.authorized(altered, 'Windows'))
        self.assertFalse(m.authorized(env, 'Linux'))

    def test_timeout_and_errors_do_not_leak(self):
        for error in (OSError('SECRET'), subprocess.TimeoutExpired('SECRET', 10), ValueError('SECRET')):
            with patch.object(m.subprocess, 'run', side_effect=error):
                result = m.collect(123, 1)
                self.assertEqual(result['unavailable'], 1)
                self.assertNotIn('SECRET', json.dumps(result))

    def test_collection_deadline_and_no_shell(self):
        with patch.object(m.subprocess, 'run', return_value=Mock(stdout='{}')) as run:
            m.collect(123, 4)
            self.assertEqual(run.call_args.kwargs['timeout'], 10)
            self.assertNotIn('shell', run.call_args.kwargs)
            self.assertEqual(run.call_args.args[0][-4:], ['-OwnedPid', '123', '-BuildStart', '4'])

    def test_build_exit_preserved_with_observer_failure(self):
        observer = Mock()
        observer.sample.side_effect = OSError('SECRET')
        child = Mock()
        child.wait.side_effect = [subprocess.TimeoutExpired('fixture', 60), 37]
        child.pid = 123
        with patch.object(m.subprocess, 'Popen', return_value=child) as launch, contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(m.supervise(['bash', 'file with spaces', '--jobs=8'], observer), 37)
        self.assertNotIn('SECRET', out.getvalue())
        launch.assert_called_once_with(['bash', 'file with spaces', '--jobs=8'])
        child.kill.assert_not_called()
        self.assertEqual(observer.sample.call_args_list[-1].args[0], 'build-exit')

    def test_start_failure_is_real_and_not_success(self):
        with patch.object(m.subprocess, 'Popen', side_effect=FileNotFoundError('fixture')):
            with self.assertRaises(FileNotFoundError):
                m.supervise(['missing-build'], None)

    def test_oversized_collector_rejected(self):
        with patch.object(m.subprocess, 'run', return_value=Mock(stdout='x' * (m.MAX_SAMPLE + 1))):
            self.assertEqual(m.collect(), {'unavailable': 1, 'truncated': True})

    def test_caps_and_stdout_precedes_write(self):
        with tempfile.TemporaryDirectory() as temp, patch.dict(m.os.environ, {'RUNNER_TEMP': temp}), patch.object(m, 'identity', return_value={'head': 'fixture'}), patch.object(m, 'collect', return_value={'memory': {'available_bytes': 8}, 'unavailable': 0}):
            observer = m.Observer()
            with contextlib.redirect_stdout(io.StringIO()) as out:
                observer.sample('before-prepare')
            self.assertIn('available_bytes', out.getvalue())
            self.assertLess(observer.path.stat().st_size, m.MAX_SAMPLE)
            observer.count = m.MAX_SAMPLES
            with contextlib.redirect_stdout(io.StringIO()):
                observer.sample('after-prepare')
            self.assertEqual(len(observer.path.read_text().splitlines()), 1)
            observer.count = 0
            observer.size = m.MAX_TOTAL
            with contextlib.redirect_stdout(io.StringIO()):
                observer.sample('after-prepare')
            self.assertEqual(len(observer.path.read_text().splitlines()), 1)

    def test_workflow_only_diagnostic_job_uses_observer(self):
        workflow = Path(__file__).parents[1] / 'workflows' / 'bazel.yml'
        before, job = workflow.read_text().split('  voice-runtime-diagnostic:', 1)
        self.assertNotIn('observe_windows_ci_resources', before)
        self.assertIn('python .github/scripts/observe_windows_ci_resources.py supervise -- bash ./.github/scripts/run-bazel-ci.sh', job)
        self.assertIn('if: github.event_name == \'workflow_dispatch\' && inputs.voice_runtime_diagnostic', job)
        self.assertEqual(job.count('if: always() && !cancelled()'), 4)
        self.assertNotIn('--jobs', job)

    def test_nested_observation_input_and_order(self):
        actions = Path(__file__).parents[1] / 'actions'
        prepare = (actions / 'prepare-bazel-ci' / 'action.yml').read_text()
        setup = (actions / 'setup-bazel-ci' / 'action.yml').read_text()
        for text in (prepare, setup):
            self.assertIn('windows-resource-observation:', text)
            self.assertIn('default: "false"', text.split('windows-resource-observation:', 1)[1])
        self.assertIn('windows-resource-observation: ${{ inputs.windows-resource-observation }}', prepare)
        self.assertLess(setup.index('id: setup_ci'), setup.index('after-provision'))
        self.assertLess(setup.index('after-provision'), setup.index('Provision pinned native Windows'))
        workflow = (Path(__file__).parents[1] / 'workflows' / 'bazel.yml').read_text()
        self.assertEqual(workflow.count('windows-resource-observation: true'), 1)
        self.assertNotIn('windows-resource-observation', workflow.split('  voice-runtime-diagnostic:')[0])

    def test_powershell_numeric_fixture_without_host_queries(self):
        script = Path(__file__).with_name('observe_windows_ci_resources.ps1').resolve()
        fixture = r'''
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
'''
        with tempfile.TemporaryDirectory() as temp:
            runner = Path(temp) / 'fixture.ps1'
            runner.write_text(fixture)
            result = subprocess.run(['pwsh', '-NoProfile', '-File', str(runner), str(script)], capture_output=True, text=True, timeout=10, check=True)
        data = json.loads(result.stdout)
        self.assertEqual(data['memory']['committed_bytes'], 200)
        self.assertEqual(data['pagefiles'][0]['allocated_bytes'], 2 * 1024 * 1024)
        self.assertEqual(len(data['pagefiles']), 16)
        self.assertTrue(data['truncated'])
        self.assertIsNone(data['volumes'][0]['free_bytes'])
        self.assertNotIn('SECRET', result.stdout)
        self.assertLess(len(result.stdout.encode()), m.MAX_SAMPLE)


if __name__ == '__main__':
    unittest.main()
