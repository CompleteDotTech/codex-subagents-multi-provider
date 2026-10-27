#!/usr/bin/env python3
"""Scoped disposable-runner storage defaults and bounded numeric receipts."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

from run_bazel_with_buildbuddy import (
    REMOTE_EXECUTION_CONFIGS,
    bazel_args_without_remote_execution,
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


def scoped(env):
    return (
        env.get("GITHUB_ACTIONS") == "true"
        and env.get("GITHUB_REPOSITORY") == FORK
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and env.get("RUNNER_OS") in ("Linux", "macOS")
        and not env.get("BUILDBUDDY_API_KEY")
    )


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
    # --strategy is a per-mnemonic map: later values replace the same mnemonic.
    # A local/sandboxed override does not select a remote execution strategy.
    strategies = {}
    for name, value in explicit:
        mnemonic, equal, choices = value.partition("=")
        if name == "--strategy" and equal:
            strategies[mnemonic] = choices.split(",")
    if (
        scoped(env)
        and args
        and args[0] in ("build", "test", "coverage", "run", "info")
        and not any(name == "--disk_cache" for name, _ in explicit)
        and not any(
            (name in ("--remote_executor", "--remote_cache") and value)
            or (name == "--config" and ("buildbuddy" in value or value == "remote"))
            for name, value in explicit
        )
        and not any("remote" in choices for choices in strategies.values())
    ):
        return ["--disk_cache="]
    return []


def local_arguments(args, env):
    """Match the existing keyless backend, including known split CI configs.

    Other routes retain their original bytes. Only configs already classified by
    the backend as credential-dependent remote CI are canonicalized and removed.
    """
    if not scoped(env):
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


def receipt(phase, args, env, status=None):
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
        "paths_are": "declared; rc/config overrides unverified",
        "cpu_count": os.cpu_count(),
        "roots": {},
    }
    seen = {}
    for role, raw in roots(args, env).items():
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
    try:
        phase, status, *args = args
        if phase not in (
            "before-build",
            "after-build",
            "before-restore",
            "after-restore",
        ):
            raise ValueError("unknown phase")
        output = receipt(phase, args, env, None if status == "none" else int(status))
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
