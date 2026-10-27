"""Job-local, bounded observation; never changes resource budgets or host state."""
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

MAX_SAMPLE = 16 * 1024
MAX_TOTAL = 2 * 1024 * 1024
MAX_SAMPLES = 120
INTERVAL = 60


def authorized(env=None, system=None):
    env = os.environ if env is None else env
    return (env.get('GITHUB_REPOSITORY') == 'CompleteDotTech/codex-subagents-multi-provider'
            and env.get('GITHUB_ACTIONS') == 'true'
            and env.get('RUNNER_OS') == 'Windows'
            and env.get('RUNNER_ENVIRONMENT') == 'github-hosted'
            and (platform.system() if system is None else system) == 'Windows')


def identity():
    def git(ref):
        try:
            value = subprocess.check_output(['git', 'rev-parse', ref], timeout=2,
                                            stderr=subprocess.DEVNULL, text=True).strip()
            return value if len(value) == 40 and all(c in '0123456789abcdef' for c in value) else None
        except (OSError, subprocess.SubprocessError):
            return None
    return dict(head=git('HEAD'), tree=git('HEAD^{tree}'),
                run=os.environ.get('GITHUB_RUN_ID'), attempt=os.environ.get('GITHUB_RUN_ATTEMPT'),
                job=os.environ.get('GITHUB_JOB'), architecture=platform.machine())


def collect(owned_pid=0, start=0):
    try:
        completed = subprocess.run(['pwsh', '-NoProfile', '-NonInteractive', '-File',
                                    str(Path(__file__).with_suffix('.ps1')),
                                    '-OwnedPid', str(owned_pid), '-BuildStart', str(start)], timeout=10,
                                   capture_output=True, text=True, check=True)
        if len(completed.stdout.encode('utf-8')) > MAX_SAMPLE:
            return dict(unavailable=1, truncated=True)
        data = json.loads(completed.stdout)
        if not isinstance(data, dict):
            raise ValueError('invalid result')
        return data
    except (OSError, subprocess.SubprocessError, ValueError):
        # Do not print exceptions: they may contain command or environment text.
        return dict(unavailable=1, truncated=False)


class Observer:
    def __init__(self):
        self.provenance = identity()
        self.path = Path(os.environ['RUNNER_TEMP']) / 'voice-runtime-diagnostic' / 'resources.jsonl'
        self.count = 0
        self.size = 0
        self.exhausted = False
        try:
            self.size = self.path.stat().st_size
            if self.size <= MAX_TOTAL:
                with self.path.open('rb') as source:
                    self.count = sum(1 for _ in source)
            else:
                self.exhausted = True
        except FileNotFoundError:
            pass
        except OSError:
            self.exhausted = True

    def sample(self, phase, owned_pid=0, start=0):
        if self.exhausted or self.count >= MAX_SAMPLES or self.size >= MAX_TOTAL:
            print('windows-resources exhausted=1', flush=True)
            return
        data = collect(owned_pid, start)
        record = dict(identity=self.provenance, phase=phase, unix_seconds=int(time.time()), **data)
        line = json.dumps(record, separators=(',', ':')).encode('utf-8') + b'\n'
        if len(line) > MAX_SAMPLE:
            record = dict(identity=self.provenance, phase=phase, unix_seconds=int(time.time()),
                          unavailable=1, truncated=True)
            line = json.dumps(record, separators=(',', ':')).encode('utf-8') + b'\n'
        # Console precedes disk I/O, including when the runner disk is full.
        memory = data.get('memory') or {}
        numeric = {key: value for key, value in memory.items()
                   if value is None or type(value) in (int, float)}
        print('windows-resources ' + json.dumps(dict(phase=phase, memory=numeric,
              volumes=data.get('volumes', []), pagefiles=data.get('pagefiles', []),
              unavailable=data.get('unavailable', 0), truncated=data.get('truncated', False)),
              separators=(',', ':'))[:1900], flush=True)
        self.count += 1
        if self.size + len(line) > MAX_TOTAL:
            self.exhausted = True
            return
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open('ab') as target:
                target.write(line)
                target.flush()
            self.size += len(line)
        except OSError:
            print('windows-resources artifact_unavailable=1', flush=True)
            # Keep bounded console observations even if disk writes fail.
            self.size += len(line)


def safe_sample(observer, phase, pid=0, start=0):
    if observer is not None:
        try:
            observer.sample(phase, pid, start)
        except Exception:
            print('windows-resources unavailable=1', flush=True)


def supervise(command, observer):
    safe_sample(observer, 'before-build')
    # Argument vector, inherited console, no shell interpolation or output rewriting.
    start = time.time()
    child = subprocess.Popen(command)
    deadline = time.monotonic() + INTERVAL
    try:
        while True:
            try:
                code = child.wait(timeout=max(0, deadline - time.monotonic()))
                break
            except subprocess.TimeoutExpired:
                safe_sample(observer, 'build', child.pid, start)
                deadline += INTERVAL
    finally:
        # No detached sampler exists. Collector subprocesses are synchronously
        # reaped (and on timeout killed) by subprocess.run. Never kill host tools.
        safe_sample(observer, 'build-exit')
    return code


def main(args):
    observer = None
    if authorized():
        try:
            observer = Observer()
        except Exception:
            print('windows-resources unavailable=1', flush=True)
    if args[:2] == ['supervise', '--'] and len(args) > 2:
        return supervise(args[2:], observer)
    if len(args) == 1 and args[0] in ('before-prepare', 'after-provision', 'after-prepare', 'after-artifacts'):
        safe_sample(observer, args[0])
        return 0
    raise SystemExit('expected phase or supervise -- command arguments')


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
