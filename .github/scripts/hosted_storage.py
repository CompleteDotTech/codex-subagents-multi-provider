#!/usr/bin/env python3
"""Scoped disposable-runner storage defaults and bounded numeric receipts."""

import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time
import uuid

from run_bazel_with_buildbuddy import (
    REMOTE_EXECUTION_CONFIGS,
    bazel_args_without_remote_execution,
    bazel_command,
)

FORK = "CompleteDotTech/codex-subagents-multi-provider"
INFO_OPTIONS = {
    "--config",
    "--platforms",
    "--host_platform",
    "--compilation_mode",
    "-c",
    "--disk_cache",
    "--repository_cache",
    "--repo_contents_cache",
    "--inject_repository",
    "--override_repository",
}
REMOTE_OPTIONS = {"--remote_executor", "--remote_cache", "--strategy"}


def policy_scoped(env):
    return (
        env.get("GITHUB_ACTIONS") == "true"
        and env.get("GITHUB_REPOSITORY") == FORK
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and env.get("RUNNER_OS") in ("Linux", "macOS", "Windows")
        and not env.get("BUILDBUDDY_API_KEY")
        and env.get("HOSTED_STORAGE_REMOTE_CREDENTIALS") != "true"
    )


def scoped(env):
    """Unix observation/query scope; Windows uses only the argument policy."""
    return policy_scoped(env) and env.get("RUNNER_OS") in ("Linux", "macOS")


def options(args, names=INFO_OPTIONS):
    """Read option/value pairs only before payload; retain repeats and empties."""
    index = 1  # the Bazel command
    while index < len(args) and args[index] != "--":
        arg = args[index]
        name, equal, value = arg.partition("=")
        if name in names:
            if not equal and index + 1 < len(args) and args[index + 1] != "--":
                index += 1
                value = args[index]
            yield name, value
        index += 1


def defaults(args, env):
    args = local_arguments(args, env)
    explicit = list(options(args, INFO_OPTIONS | REMOTE_OPTIONS))
    # These are scalar options: each endpoint's final value is authoritative.
    endpoints = {
        name: value
        for name, value in explicit
        if name in ("--remote_executor", "--remote_cache")
    }
    # --strategy is a per-mnemonic map: later values replace the same mnemonic.
    # A local/sandboxed override does not select a remote execution strategy.
    strategies = {}
    for name, value in explicit:
        mnemonic, equal, choices = value.partition("=")
        if name == "--strategy" and equal:
            strategies[mnemonic] = choices.split(",")
    if (
        policy_scoped(env)
        and args
        and args[0] in ("build", "test", "coverage", "run", "info")
        and not any(name == "--disk_cache" for name, _ in explicit)
        and not any(
            name == "--config" and ("buildbuddy" in value or value == "remote")
            for name, value in explicit
        )
        and not any(endpoints.values())
        and not any("remote" in choices for choices in strategies.values())
    ):
        return ["--disk_cache="]
    return []


def local_arguments(args, env):
    """Match the existing keyless backend, including known split CI configs.

    Other routes retain their original bytes. Only configs already classified by
    the backend as credential-dependent remote CI are canonicalized and removed.
    """
    if not policy_scoped(env):
        return list(args)
    normalized = []
    index = 0
    while index < len(args):
        if args[index] == "--":
            normalized.extend(args[index:])
            break
        if (
            args[index] == "--config"
            and index + 1 < len(args)
            and "--config=" + args[index + 1] in REMOTE_EXECUTION_CONFIGS
        ):
            normalized.append("--config=" + args[index + 1])
            index += 2
        else:
            normalized.append(args[index])
            index += 1
    return bazel_args_without_remote_execution(normalized)


def effective_arguments(args, env):
    args = local_arguments(args, env)
    if not args:
        return args
    return [args[0], *defaults(args, env), *args[1:]]


def info_options(args):
    return [
        name + "=" + value if name != "-c" else "--compilation_mode=" + value
        for name, value in options(args)
    ]


def roots(args, env):
    selected = dict(options(args))
    disk = selected.get("--disk_cache", str(Path.home() / ".cache/bazel-disk-cache"))
    return {
        "output_base": env.get("BAZEL_OUTPUT_BASE"),
        "repository_cache": selected.get(
            "--repository_cache", env.get("BAZEL_REPOSITORY_CACHE")
        ),
        "repo_contents_cache": selected.get(
            "--repo_contents_cache", env.get("BAZEL_REPO_CONTENTS_CACHE")
        ),
        "disk_cache": disk,
        "ci_temp": env.get("TMP"),
        "runner_temp": env.get("RUNNER_TEMP"),
        "workspace_archive": env.get("GITHUB_WORKSPACE"),
    }


def measure(path):
    """No traversal listings, symlink following, environment or process dumps."""
    result = {"exists": path.exists()}
    ancestor = path
    while not ancestor.exists() and ancestor != ancestor.parent:
        ancestor = ancestor.parent
    stat = ancestor.stat()
    fs = os.statvfs(ancestor)
    result.update(
        device=stat.st_dev,
        total_bytes=fs.f_blocks * fs.f_frsize,
        free_bytes=fs.f_bfree * fs.f_frsize,
        available_bytes=fs.f_bavail * fs.f_frsize,
        available_inodes=fs.f_favail,
    )
    if result["exists"]:
        try:
            # -s emits one total, -k is supported by GNU and BSD du; no -L.
            run = subprocess.run(
                ["du", "-sk", str(path)],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                timeout=2,
                check=False,
            )
            if run.returncode == 0 and len(run.stdout) <= 4096:
                result["allocated_bytes"] = int(run.stdout.split(None, 1)[0]) * 1024
            else:
                result["allocation_unavailable"] = True
        except (OSError, ValueError, subprocess.TimeoutExpired):
            result["allocation_unavailable"] = True
    return result


def bounded_query(command, env, timeout=15, limit=16384):
    """Bound the query client, stdout and pipe lifetime on Linux and macOS."""
    child = subprocess.Popen(
        command,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    data = bytearray()
    deadline = time.monotonic() + timeout
    finished = False
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    return "timeout", None
                chunk = os.read(child.stdout.fileno(), 4096)
                if not chunk:
                    break
                if len(data) + len(chunk) > limit:
                    return "output-limit", None
                data.extend(chunk)
        code = child.wait(timeout=max(0.01, deadline - time.monotonic()))
        finished = True
        return ("ok", bytes(data)) if code == 0 else ("failed", None)
    except subprocess.TimeoutExpired:
        return "timeout", None
    finally:
        try:
            if not finished:
                # Bazelisk may spawn a client. The new process group belongs
                # only to this query, not the existing build server/session.
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if child.poll() is None:
                    child.wait(timeout=2)
        finally:
            child.stdout.close()


def actual_paths(args, env):
    if not scoped(env):
        return "out-of-scope", None
    keys = ("output_base", "output_path", "execution_root")
    # Same backend/startup normalization, cwd and effective local configuration
    # as BUILD. Only safe info-compatible options, never arbitrary build flags.
    command = bazel_command(
        "--noexperimental_remote_repo_contents_cache",
        "info",
        *info_options(effective_arguments(args, env)),
        *keys,
        env=env,
    )
    try:
        state, raw = bounded_query(command, env)
        if state != "ok":
            return state, None
        values = {}
        for line in raw.decode("utf-8").splitlines():
            key, separator, value = line.partition(": ")
            if (
                key not in keys
                or not separator
                or key in values
                or not Path(value).is_absolute()
            ):
                return "invalid-output", None
            values[key] = value
        return ("ok", values) if set(values) == set(keys) else ("incomplete", None)
    except (OSError, ValueError, UnicodeError, subprocess.TimeoutExpired):
        return "unavailable", None


def receipt(phase, args, env, status=None, verified=None, query_state=None):
    output = {
        "schema": 1,
        "phase": phase,
        "time_unix": time.time(),
        "command": args[0]
        if args and args[0] in ("build", "test", "coverage", "run")
        else "setup",
        "exit_code": status,
        "runner_os": env.get("RUNNER_OS"),
        "run_id": env.get("GITHUB_RUN_ID"),
        "attempt": env.get("GITHUB_RUN_ATTEMPT"),
        "job": env.get("GITHUB_JOB"),
        "paths_are": "actual bounded bazel info"
        if verified
        else "declared; rc/config overrides unverified",
        "query_state": query_state,
        "cache_save_requested": env.get("HOSTED_STORAGE_CACHE_SAVE") == "true"
        if "HOSTED_STORAGE_CACHE_SAVE" in env
        else None,
        "cpu_count": os.cpu_count(),
        "roots": {},
    }
    seen = {}
    # A path-query receipt contains only the three verified roots; other phases
    # retain the seven declared roles. Never sum overlapping directory totals.
    for role, raw in (verified if verified else roots(args, env)).items():
        if not raw:
            output["roots"][role] = {"disabled_or_unavailable": True}
            continue
        try:
            path = Path(raw).expanduser().resolve()
            entry = {"path": str(path)}
            if path in seen:
                entry["alias_of"] = seen[path]
            else:
                entry.update(measure(path))
                entry["overlaps"] = [
                    other
                    for prior, other in seen.items()
                    if path in prior.parents or prior in path.parents
                ]
                seen[path] = role
            output["roots"][role] = entry
        except (OSError, ValueError):
            output["roots"][role] = {"unavailable": True}
    # Only commit identities are captured, never git configuration or env values.
    for key, expression in (("head", "HEAD"), ("tree", "HEAD^{tree}")):
        try:
            value = (
                subprocess.run(
                    ["git", "rev-parse", expression],
                    capture_output=True,
                    timeout=2,
                    check=False,
                )
                .stdout.decode()
                .strip()
            )
            if len(value) == 40 and all(c in "0123456789abcdef" for c in value):
                output[key] = value
        except (OSError, UnicodeError, subprocess.TimeoutExpired):
            pass
    return output


def main():
    mode, *args = sys.argv[1:]
    env = dict(os.environ)
    if mode in ("defaults", "effective", "info"):
        if mode == "defaults":
            values = defaults(args, env)
        elif mode == "effective":
            values = effective_arguments(args, env)
        else:
            values = info_options(effective_arguments(args, env))
        for value in values:
            sys.stdout.buffer.write(value.encode() + b"\0")
        return
    if not scoped(env):
        return
    if env.get("GITHUB_OUTPUT"):
        try:
            with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                stream.write("enabled=true\n")
        except OSError:
            # An exhausted output filesystem must not prevent stdout metrics.
            pass
    try:
        phase, status, *args = args
        if phase not in (
            "before-build",
            "after-build",
            "before-restore",
            "after-restore",
            "before-cache-save",
            "after-cache-save",
            "after-build-paths",
        ):
            raise ValueError("unknown phase")
        state, verified = actual_paths(args, env) if mode == "resolve" else (None, None)
        output = receipt(
            phase, args, env, None if status == "none" else int(status), verified, state
        )
        text = json.dumps(output, sort_keys=True)
        if len(text.encode()) > 32768:
            raise ValueError("receipt bound exceeded")
        # Print first: a disk-full write can prevent saving the JSON receipt.
        print(
            "Hosted storage: "
            + json.dumps(
                {
                    "phase": phase,
                    "exit_code": output["exit_code"],
                    "available_bytes": {
                        k: v.get("available_bytes") for k, v in output["roots"].items()
                    },
                }
            ),
            flush=True,
        )
        destination = Path(env["RUNNER_TEMP"]) / "hosted-storage"
        destination.mkdir(exist_ok=True)
        (destination / (phase + "-" + uuid.uuid4().hex + ".json")).write_text(
            text, encoding="utf-8"
        )
    except Exception:
        # Observation failure must never replace the original build outcome.
        print("Hosted storage observation unavailable", file=sys.stderr)


if __name__ == "__main__":
    main()
