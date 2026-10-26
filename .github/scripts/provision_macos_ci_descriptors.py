"""Provision descriptor capacity only on this fork's disposable macOS CI hosts."""

import os
import subprocess
import sys

FORK = "CompleteDotTech/codex-subagents-multi-provider"
MINIMUM = 16384


def read_limit(name):
    result = subprocess.run(
        ["/usr/sbin/sysctl", "-n", name],
        check=True,
        capture_output=True,
        text=True,
        timeout=15,
    )
    value = result.stdout.strip()
    if not value.isascii() or not value.isdecimal() or int(value) <= 0:
        raise ValueError(f"Invalid numeric readback for {name}")
    return int(value)


def main():
    if not (
        sys.platform == "darwin"
        and os.environ.get("RUNNER_OS") == "macOS"
        and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and os.environ.get("GITHUB_ACTIONS") == "true"
        and os.environ.get("GITHUB_REPOSITORY") == FORK
    ):
        print(
            "macOS descriptor provisioning skipped outside authorized disposable fork CI"
        )
        return
    before = {
        name: read_limit(name) for name in ("kern.maxfiles", "kern.maxfilesperproc")
    }
    print(f"macOS descriptor limits before: {before}", flush=True)
    # Raise the global ceiling first if required; never reduce an existing limit.
    # The isolated test raises only its own RLIMIT_NOFILE and retains all strict
    # >=10240 descriptor, EBADF and compatible-fallback assertions.
    required = max(MINIMUM, before["kern.maxfilesperproc"])
    for name in ("kern.maxfiles", "kern.maxfilesperproc"):
        target = max(before[name], required)
        if before[name] < target:
            subprocess.run(
                ["sudo", "-n", "/usr/sbin/sysctl", "-w", f"{name}={target}"],
                check=True,
                timeout=15,
            )
            actual = read_limit(name)
            print(f"macOS descriptor limit write readback: {name}={actual}", flush=True)
            if actual < target:
                raise RuntimeError(
                    f"Descriptor provisioning readback below requested {name}={target}"
                )
    after = {name: read_limit(name) for name in before}
    print(f"macOS descriptor limits after: {after}", flush=True)
    if (
        after["kern.maxfilesperproc"] < required
        or after["kern.maxfiles"] < after["kern.maxfilesperproc"]
    ):
        raise RuntimeError("Disposable macOS CI descriptor capacity is insufficient")


if __name__ == "__main__":
    main()
