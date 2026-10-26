#!/usr/bin/env python3
"""Build separately locked old/new Clatter endpoints and verify relay wire compatibility.

Requires Cargo and its normal dependency fetch access. All generated projects and
artifacts live outside the repository; no credentials or remote services are used.
"""

import argparse
import hashlib
import json
import queue
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

FIXTURES = Path(__file__).resolve().parent / "fixtures" / "noise_interop"


def project(root, version, cargo):
    destination = root / version
    (destination / "src").mkdir(parents=True, exist_ok=True)
    for source, target in [
        (version + ".toml.in", "Cargo.toml"),
        (version + ".lock.in", "Cargo.lock"),
        ("endpoint.rs.in", "src/main.rs"),
    ]:
        shutil.copyfile(FIXTURES / source, destination / target)
    executable_name = "noise-interop-" + version
    result = subprocess.run(
        [
            cargo,
            "build",
            "--locked",
            "-j2",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
            str(destination / "Cargo.toml"),
            "--bin",
            executable_name,
        ],
        check=True,
        timeout=900,
        stdout=subprocess.PIPE,
        text=True,
    )
    artifacts = [
        message["executable"]
        for line in result.stdout.splitlines()
        if (message := json.loads(line)).get("reason") == "compiler-artifact"
        and message.get("target", {}).get("name") == executable_name
        and message.get("executable")
    ]
    require(
        len(artifacts) == 1, "Cargo did not identify exactly one endpoint executable"
    )
    executable = Path(artifacts[0]).resolve()
    require(executable.is_file(), "Cargo endpoint executable does not exist")
    return executable


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def read(process):
    result = queue.Queue()
    thread = threading.Thread(
        target=lambda: result.put(process.stdout.readline()), daemon=True
    )
    thread.start()
    try:
        line = result.get(timeout=30)
    except queue.Empty as error:
        raise TimeoutError(
            "endpoint produced no protocol line within 30 seconds"
        ) from error
    require(bool(line), "endpoint stopped before completing protocol")
    return json.loads(line)


def send(process, value):
    process.stdin.write(json.dumps(value) + "\n")
    process.stdin.flush()


def exchange(initiator_binary, responder_binary):
    processes = []
    watchdog = None
    try:
        for binary, role in [
            (initiator_binary, "initiator"),
            (responder_binary, "responder"),
        ]:
            processes.append(
                subprocess.Popen(
                    [str(binary), role],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    text=True,
                )
            )
        initiator, responder = processes
        watchdog = threading.Timer(
            30,
            lambda: [process.kill() for process in processes if process.poll() is None],
        )
        watchdog.start()
        initiator_key, responder_key = read(initiator), read(responder)
        require(
            len(initiator_key[0]) == len(responder_key[0]) == 32,
            "X25519 key size changed",
        )
        require(
            len(initiator_key[1]) == len(responder_key[1]) == 1184,
            "ML-KEM key size changed",
        )
        send(initiator, responder_key)
        send(responder, initiator_key)
        send(responder, read(initiator))
        send(initiator, read(responder))
        outgoing = [read(process) for process in processes]
        for index, process in enumerate(processes):
            incoming, second = outgoing[1 - index]
            require(
                len(incoming) == len(b"encrypted-json-rpc") + 16,
                "record/tag size changed",
            )
            send(process, incoming)
            altered = second.copy()
            altered[-1] ^= 1
            send(process, altered)
        for process in processes:
            require(
                read(process) == "authenticated-transport-and-tamper-rejection",
                "endpoint verification failed",
            )
            require(process.wait(timeout=15) == 0, "endpoint exited unsuccessfully")
    finally:
        if watchdog is not None:
            watchdog.cancel()
        for process in processes:
            if process.poll() is None:
                process.kill()
            process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="codex-noise-interop-") as temporary:
        root = (args.work_dir or Path(temporary)).resolve()
        require(
            not root.is_relative_to(FIXTURES.parents[3]),
            "work directory must be outside the repository",
        )
        root.mkdir(parents=True, exist_ok=True)
        binaries = {
            version: project(root, version, args.cargo) for version in ("old", "new")
        }
        for initiator, responder in [("old", "new"), ("new", "old")]:
            exchange(binaries[initiator], binaries[responder])
            print(
                f"PASS {initiator} initiator / {responder} responder: authenticated handshake, bidirectional transport, tamper rejection"
            )
        print(
            json.dumps(
                {
                    path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in sorted(FIXTURES.iterdir())
                },
                sort_keys=True,
            )
        )


if __name__ == "__main__":
    main()
