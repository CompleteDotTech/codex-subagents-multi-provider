"""Capture bounded Windows workspace provenance after a failed CI build.

This observes existing outputs and replays a known status producer; it never
repairs receipts or supplies a commit to the failed build.
"""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

from run_bazel_with_buildbuddy import bazel_command

FORK = "CompleteDotTech/codex-subagents-multi-provider"


def capture_file(source, destination, limit):
    if not source.is_file():
        return {"present": False}
    size = source.stat().st_size
    if size > limit:
        return {"present": True, "size": size, "captured": False}
    data = source.read_bytes()
    destination.write_bytes(data)
    return {
        "present": True,
        "size": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def command(argv, cwd):
    try:
        result = subprocess.run(argv, cwd=cwd, capture_output=True, timeout=30)
        return result.returncode, result.stdout, result.stderr
    except (OSError, subprocess.TimeoutExpired) as error:
        # Exception strings may contain command arguments; do not serialize them.
        return None, b"", type(error).__name__.encode()


def main():
    if not (
        os.environ.get("RUNNER_OS") == "Windows"
        and os.environ.get("GITHUB_ACTIONS") == "true"
        and os.environ.get("GITHUB_REPOSITORY") == FORK
    ):
        return
    output = os.environ.get("CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR")
    base = os.environ.get("BAZEL_OUTPUT_BASE")
    if not output or not base:
        return
    root = Path.cwd()
    destination = Path(output) / "workspace-provenance" / str(os.getpid())
    destination.mkdir(parents=True, exist_ok=True)
    report = {
        "cwd": str(root),
        "observation": "after failed build; no receipt modified",
    }
    for name, args in (
        ("head", ["rev-parse", "--verify", "HEAD"]),
        ("tree", ["rev-parse", "--verify", "HEAD^{tree}"]),
    ):
        code, stdout, _ = command(["git", *args], root)
        value = stdout.decode("ascii", errors="replace").strip()
        report[name] = {
            "exit": code,
            "value": value if len(value) <= 64 else "oversized",
        }
    execroot = Path(base) / "execroot" / "_main"
    status = execroot / "bazel-out" / "stable-status.txt"
    report["ctx.info_file"] = {
        "path": str(status),
        **capture_file(status, destination / "stable-status.txt", 65536),
    }
    receipt = (
        execroot
        / "bazel-out"
        / "windows_x86_64_msvc-fastbuild"
        / "bin"
        / "third_party"
        / "voice"
        / "native_prefix_windows_x86_64"
        / "built.json"
    )
    report["native_receipt"] = {
        "path": str(receipt),
        **capture_file(receipt, destination / "built.json", 1024 * 1024),
    }
    # Canonicalize the original build flags without starting another build. The
    # helper applies the same startup/remote defaults. Never persist raw output:
    # unrelated effective flags can contain remote authentication headers.
    args = sys.argv[1:]
    code, stdout, _ = command(
        bazel_command("canonicalize-flags", "--for_command=build", "--", *args), root
    )
    allowed = (
        "--workspace_status_command=",
        "--shell_executable=",
        "--enable_platform_specific_config=",
        "--platforms=",
        "--host_platform=",
    )
    effective = [
        line
        for line in stdout.decode("utf-8", errors="replace").splitlines()
        if line.startswith(allowed) and len(line) <= 4096
    ]
    report["effective_flags"] = {
        "exit": code,
        "flags": effective,
        "method": "canonicalize-flags --for_command=build with original build flags",
    }
    selected = [
        line.partition("=")[2]
        for line in effective
        if line.startswith("--workspace_status_command=")
    ]
    report["status_producer"] = {
        "selected": selected,
        "replay_is_separate_observation": True,
    }
    # Replay only the repository's known producer, never arbitrary effective code.
    # cmd /d avoids host AutoRun; this does not claim to reproduce Bazel's launcher.
    if selected and selected[-1].replace("\\", "/") in (
        "./scripts/workspace-status.cmd",
        "scripts/workspace-status.cmd",
    ):
        argv = [
            os.environ.get("COMSPEC", "cmd.exe"),
            "/d",
            "/c",
            "scripts\\workspace-status.cmd",
        ]
        code, stdout, stderr = command(argv, root)
        report["status_producer"].update({"replay_argv": argv, "exit": code})
        for name, data in (("stdout", stdout), ("stderr", stderr)):
            if len(data) <= 65536:
                (destination / f"status-producer-{name}.bin").write_bytes(data)
    (destination / "observation.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    print(
        "Windows workspace provenance captured; see execution-log artifact/workspace-provenance (observation only)."
    )


if __name__ == "__main__":
    main()
