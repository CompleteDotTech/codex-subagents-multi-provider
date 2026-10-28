"""Job-local, bounded observation; never changes resource budgets or host state."""

import ctypes
import json
import math
import re
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
import threading

MAX_SAMPLE = 16 * 1024
MAX_TOTAL = 2 * 1024 * 1024
MAX_SAMPLES = 120
INTERVAL = 60


def authorized(env=None, system=None):
    env = os.environ if env is None else env
    return (
        env.get("GITHUB_REPOSITORY") == "CompleteDotTech/codex-subagents-multi-provider"
        and env.get("GITHUB_ACTIONS") == "true"
        and env.get("RUNNER_OS") == "Windows"
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and (platform.system() if system is None else system) == "Windows"
    )


def identity():
    def git(ref):
        try:
            value = subprocess.check_output(
                ["git", "rev-parse", ref],
                timeout=2,
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
            return (
                value
                if len(value) == 40 and all(c in "0123456789abcdef" for c in value)
                else None
            )
        except (OSError, subprocess.SubprocessError):
            return None

    return dict(
        head=git("HEAD"),
        tree=git("HEAD^{tree}"),
        run=os.environ.get("GITHUB_RUN_ID"),
        attempt=os.environ.get("GITHUB_RUN_ATTEMPT"),
        job=os.environ.get("GITHUB_JOB"),
        architecture=platform.machine(),
    )


def bounded_output(command, timeout=10):
    """Read at most MAX_SAMPLE bytes; discard stderr, kill/reap only our collector.

    The collector script starts no subprocesses. Its owned stdout reader therefore
    reaches EOF when the collector exits or is killed, and is joined before return.
    """
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + timeout
    data = bytearray()
    done = threading.Event()
    overflow = threading.Event()
    failed = threading.Event()

    def read():
        try:
            while True:
                chunk = child.stdout.read(1024)
                if not chunk:
                    break
                remaining = MAX_SAMPLE - len(data)
                data.extend(chunk[:remaining])
                if len(chunk) > remaining:
                    overflow.set()
                    break
        except OSError:
            failed.set()
        finally:
            done.set()

    reader = threading.Thread(target=read, name="windows-resource-collector-reader")
    try:
        reader.start()
        if not done.wait(max(0, deadline - time.monotonic())):
            raise subprocess.TimeoutExpired("resource collector", timeout)
        if overflow.is_set():
            raise OverflowError("resource collector limit")
        if failed.is_set():
            raise OSError("resource collector read")
        child.wait(timeout=max(0.001, deadline - time.monotonic()))
        if child.returncode:
            raise ValueError("resource collector failed")
        return bytes(data)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
        if reader.ident is not None:
            reader.join()
        child.stdout.close()


def owned_creation_microseconds(child, api=None):
    """GetProcessTimes on Popen's OWNED handle, microseconds since 1601 UTC.

    CIM CreationDate has microsecond precision; truncate the Windows FILETIME's
    100 ns fraction once. Do not infer identity from PID or launch wall clocks.
    """
    try:
        if api is None:
            if platform.system() != "Windows":
                return None
            api = ctypes.WinDLL("kernel32", use_last_error=True).GetProcessTimes
            api.argtypes = [ctypes.c_void_p] * 5
            api.restype = ctypes.c_int
        values = [ctypes.c_uint64() for _ in range(4)]
        if not api(int(child._handle), *(ctypes.byref(value) for value in values)):
            return None
        return values[0].value // 10
    except (AttributeError, OSError, TypeError, ValueError):
        return None


def sanitize(data):
    """Only a fixed numeric schema and fixed labels may reach logs/artifacts."""
    if not isinstance(data, dict):
        raise ValueError("invalid collector schema")

    def number(value):
        return (
            value
            if type(value) in (int, float) and math.isfinite(value) and value >= 0
            else None
        )

    def fields(row, names):
        return {name: number(row.get(name)) for name in names}

    def volume(value):
        return (
            value.rstrip("\\").upper()
            if isinstance(value, str) and re.fullmatch(r"[A-Za-z]:\\?", value)
            else None
        )

    result = dict(
        unavailable=number(data.get("unavailable")) or 0,
        truncated=data.get("truncated") is True,
        process_scope="live-owned-descendant-snapshot-only-detached-servers-unobserved",
    )
    memory = data.get("memory")
    result["memory"] = (
        fields(
            memory,
            (
                "available_bytes",
                "committed_bytes",
                "commit_limit_bytes",
                "commit_peak_bytes",
                "percent_commit",
            ),
        )
        if isinstance(memory, dict)
        else None
    )
    tables = {
        "volumes": (16, ("free_bytes", "total_bytes")),
        "pagefiles": (16, ("allocated_bytes", "current_bytes", "peak_bytes")),
        "configured_pagefiles": (16, ("initial_bytes", "maximum_bytes")),
        "processes": (128, ("working_bytes", "private_bytes")),
    }
    for name, (limit, names) in tables.items():
        rows = data.get(name)
        if not isinstance(rows, list):
            result[name] = None
            continue
        result[name] = []
        if len(rows) > limit:
            result["truncated"] = True
        for row in rows[:limit]:
            if not isinstance(row, dict):
                result["unavailable"] += 1
                continue
            cleaned = fields(row, names)
            if name == "processes":
                image = row.get("image")
                if not isinstance(image, str) or not re.fullmatch(
                    r"(rustc|link|cl|ninja|cmake|python|python3|bazel|bash)(\.exe)?",
                    image,
                ):
                    result["unavailable"] += 1
                    continue
                cleaned["image"] = image
            else:
                cleaned["volume"] = volume(row.get("volume"))
                if cleaned["volume"] is None:
                    result["unavailable"] += 1
            result[name].append(cleaned)
    vhd = data.get("vhd")
    result["vhd"] = None
    if isinstance(vhd, dict):
        result["vhd"] = fields(
            vhd, ("file_length_bytes", "logical_size_bytes", "allocated_physical_bytes")
        )
        result["vhd"].update(
            backing_volume=volume(vhd.get("backing_volume")),
            logical_size_source="setup-dev-drive.ps1",
        )
    return result


def collect(owned_pid=0, start=None):
    try:
        output = bounded_output(
            [
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-File",
                str(Path(__file__).with_suffix(".ps1")),
                "-OwnedPid",
                str(owned_pid),
                "-CreationMicroseconds",
                str(start or 0),
            ],
            timeout=10,
        )
        return sanitize(json.loads(output))
    except OverflowError:
        return dict(unavailable=1, truncated=True)
    except (OSError, subprocess.SubprocessError, ValueError):
        # Do not print exceptions: they may contain command or environment text.
        return dict(unavailable=1, truncated=False)


class Observer:
    def __init__(self):
        self.provenance = identity()
        self.path = (
            Path(os.environ["RUNNER_TEMP"])
            / "voice-runtime-diagnostic"
            / "resources.jsonl"
        )
        self.count = 0
        self.size = 0
        self.exhausted = False
        try:
            self.size = self.path.stat().st_size
            if self.size <= MAX_TOTAL:
                with self.path.open("rb") as source:
                    self.count = sum(1 for _ in source)
            else:
                self.exhausted = True
        except FileNotFoundError:
            pass
        except OSError:
            self.exhausted = True

    def sample(self, phase, owned_pid=0, start=0):
        if self.exhausted or self.count >= MAX_SAMPLES or self.size >= MAX_TOTAL:
            print("windows-resources exhausted=1", flush=True)
            return
        data = sanitize(collect(owned_pid, start))
        record = dict(
            identity=self.provenance, phase=phase, unix_seconds=int(time.time()), **data
        )
        line = json.dumps(record, separators=(",", ":")).encode("utf-8") + b"\n"
        if len(line) > MAX_SAMPLE:
            record = dict(
                identity=self.provenance,
                phase=phase,
                unix_seconds=int(time.time()),
                unavailable=1,
                truncated=True,
            )
            line = json.dumps(record, separators=(",", ":")).encode("utf-8") + b"\n"
        # Console precedes disk I/O, including when the runner disk is full.
        memory = data.get("memory") or {}
        numeric = {
            key: value
            for key, value in memory.items()
            if value is None or type(value) in (int, float)
        }
        print(
            "windows-resources "
            + json.dumps(
                dict(
                    phase=phase,
                    memory=numeric,
                    volumes=data.get("volumes", []),
                    pagefiles=data.get("pagefiles", []),
                    unavailable=data.get("unavailable", 0),
                    truncated=data.get("truncated", False),
                ),
                separators=(",", ":"),
            )[:1900],
            flush=True,
        )
        self.count += 1
        if self.size + len(line) > MAX_TOTAL:
            self.exhausted = True
            return
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open("ab") as target:
                target.write(line)
                target.flush()
            self.size += len(line)
        except OSError:
            print("windows-resources artifact_unavailable=1", flush=True)
            # Keep bounded console observations even if disk writes fail.
            self.size += len(line)


def safe_sample(observer, phase, pid=0, start=0):
    if observer is not None:
        try:
            observer.sample(phase, pid, start)
        except Exception:
            print("windows-resources unavailable=1", flush=True)


def supervise(command, observer):
    safe_sample(observer, "before-build")
    # Argument vector, inherited console, no shell interpolation or output rewriting.
    child = subprocess.Popen(command)
    start = owned_creation_microseconds(child)
    deadline = time.monotonic() + INTERVAL
    try:
        while True:
            try:
                code = child.wait(timeout=max(0, deadline - time.monotonic()))
                break
            except subprocess.TimeoutExpired:
                safe_sample(observer, "build", child.pid, start)
                deadline += INTERVAL
    finally:
        # No detached sampler exists. Collector subprocesses are synchronously
        # reaped (and on timeout killed) by bounded_output. Never kill host tools.
        safe_sample(observer, "build-exit")
    return code


def main(args):
    observer = None
    if authorized():
        try:
            observer = Observer()
        except Exception:
            print("windows-resources unavailable=1", flush=True)
    if args[:2] == ["supervise", "--"] and len(args) > 2:
        return supervise(args[2:], observer)
    if len(args) == 1 and args[0] in (
        "before-prepare",
        "after-provision",
        "after-prepare",
        "after-artifacts",
    ):
        safe_sample(observer, args[0])
        return 0
    raise SystemExit("expected phase or supervise -- command arguments")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
