"""Bounded, manifest-driven Windows PE and unchanged-environment startup evidence."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import threading
import time

MAX_FILE = 512 * 1024 * 1024


def resolve(key, env):
    if key.startswith(("/", "\\")) or ".." in key.split("/") or ":" in key:
        raise ValueError("unsafe runfile key")
    manifest = env.get("RUNFILES_MANIFEST_FILE")
    if manifest:
        with open(manifest, encoding="utf-8") as stream:
            for line in stream:
                name, _, physical = line.rstrip("\n").partition(" ")
                if name == key:
                    return Path(physical or name).resolve(strict=True)
        raise ValueError("missing manifest entry: " + key)
    return (Path(env["RUNFILES_DIR"]) / key).resolve(strict=True)


def digest(path):
    if path.stat().st_size > MAX_FILE:
        raise ValueError("PE file size bound exceeded")
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def run(command, env, cwd, limit=4096):
    result = {
        "command_kind": Path(command[0]).name,
        "executable": command[0],
        "cwd": str(cwd),
    }
    try:
        child = subprocess.Popen(
            command, env=env, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
    except OSError as error:
        return dict(result, spawn_error=str(error))
    buffers = [bytearray(), bytearray()]
    truncated = [False, False]

    def read(stream, index):
        with stream:
            while chunk := stream.read(4096):
                room = limit - len(buffers[index])
                buffers[index].extend(chunk[:room])
                truncated[index] |= len(chunk) > room

    readers = [
        threading.Thread(target=read, args=(stream, i), daemon=True)
        for i, stream in enumerate((child.stdout, child.stderr))
    ]
    for reader in readers:
        reader.start()
    try:
        code = child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        child.kill()
        code = child.wait(timeout=5)
        result["timeout"] = True
    for reader in readers:
        reader.join(timeout=1)
    result.update(
        code=code,
        signed_code=code if code < 2**31 else code - 2**32,
        code_u32_hex=f"0x{code & 0xFFFFFFFF:08x}",
        stdout=buffers[0].decode("utf-8", errors="replace"),
        stderr=buffers[1].decode("utf-8", errors="replace"),
        truncated=any(truncated),
        reader_incomplete=any(r.is_alive() for r in readers),
    )
    return result


def parse_pe(text):
    machine = re.search(r"^\s*([0-9A-Fa-f]+) machine \(([^)]+)\)", text, re.M)
    if not machine or not re.search(r"File Type:\s*(DLL|EXECUTABLE IMAGE)", text):
        raise ValueError("missing PE machine/image header")
    imports, deferred = [], []
    section = None
    for line in text.splitlines():
        if "following delay load dependencies" in line.lower():
            section = deferred
        elif "following dependencies" in line.lower():
            section = imports
        elif line.strip() == "Summary":
            section = None
        elif section is not None and re.fullmatch(r"\s*[\w.+-]+\.dll\s*", line, re.I):
            section.append(line.strip())
    return {
        "machine": machine.group(1),
        "machine_name": machine.group(2),
        "imports": imports,
        "delay_imports": deferred,
        "image_kind": "dll" if re.search(r"File Type:\s*DLL", text) else "exe",
    }


def inventory(paths):
    result = {}
    for path in paths:
        name = path.name.lower()
        if name in result:
            raise ValueError("ambiguous declared runtime basename: " + name)
        result[name] = path
    return result


def fixture(root, helper, runtime):
    destination = root / "codex-resources" / "voice" / "bin"
    destination.mkdir(parents=True)
    (root / "bin").mkdir()
    (root / "bin" / "codex.exe").write_bytes(b"")
    (root / "codex-package.json").write_text("{}")
    copied = destination / helper.name
    shutil.copyfile(helper, copied)
    for path in runtime:
        # installed_client copies every file in native_link/bin, not runtime.json.
        if path.parent.name == "bin":
            shutil.copyfile(path, destination / path.name)
    if digest(helper) != digest(copied):
        raise ValueError("installed helper hash mismatch")
    return copied


def diagnose(config, env, output):
    helper, unit, dumpbin, status = [
        resolve(config[key], env) for key in ("helper", "unit", "dumpbin", "status")
    ]
    runtime = [resolve(key, env) for key in config["runtime"]]
    declared = inventory(runtime)
    commit = re.findall(r"^STABLE_GIT_COMMIT ([0-9a-f]{40})$", status.read_text(), re.M)
    if len(commit) != 1:
        raise ValueError("actual workspace status lacks unique build commit")
    output.update(
        expected_build_commit=commit[0],
        dumpbin_sha256=digest(dumpbin),
        declared_runtime={
            name: {"path": str(path), "sha256": digest(path)}
            for name, path in declared.items()
        },
    )
    unrelated = Path(env["TEST_TMPDIR"]) / "unrelated-cwd"
    unrelated.mkdir()
    copied = fixture(Path(env["TEST_TMPDIR"]) / "installed-fixture", helper, runtime)
    output["probes"] = [
        run([str(path), arg], env, unrelated)
        for path, arg in (
            (helper, "--build-commit"),
            (copied, "--build-commit"),
            (unit, "--list"),
        )
    ]
    output["environment_identical"] = True
    output["helper_sha256"] = digest(helper)
    output["installed_sha256"] = digest(copied)
    output["unit_sha256"] = digest(unit)
    output["helper_path"] = str(helper)
    adjacent = [
        p
        for p in helper.parent.iterdir()
        if p.is_file() and p.suffix.lower() in (".dll", ".exe")
    ]
    if len(adjacent) > 128:
        raise ValueError("adjacency inventory bound exceeded")
    output["adjacent"] = {
        p.name: {"sha256": digest(p), "declared": p.name.lower() in declared}
        for p in adjacent
    }
    tool_env = dict(env, **config["tool_environment"])
    deadline = time.monotonic() + 100
    # Plugins are loaded dynamically; inspect their declared imports too.
    queue = [helper, unit] + [p for p in runtime if p.suffix.lower() == ".dll"]
    seen, total = set(), 0
    output["pe"] = []
    while queue:
        path = queue.pop(0)
        if path in seen:
            continue
        if len(seen) >= 128 or total >= 1024 * 1024 or time.monotonic() >= deadline:
            raise ValueError("PE traversal/output bound exceeded")
        seen.add(path)
        digest(path)
        result = run(
            [str(dumpbin), "/nologo", "/headers", "/dependents", str(path)],
            tool_env,
            unrelated,
            min(32768, (1024 * 1024 - total) // 2),
        )
        total += len(result.get("stdout", "").encode()) + len(
            result.get("stderr", "").encode()
        )
        if (
            result.get("code") != 0
            or result.get("truncated")
            or result.get("timeout")
            or result.get("reader_incomplete")
        ):
            output["pe"].append({"path": str(path), "inspection": result})
            raise ValueError("incomplete dumpbin inspection")
        parsed = parse_pe(result["stdout"])
        edges = []
        for name in parsed["imports"] + parsed["delay_imports"]:
            dependency = declared.get(name.lower())
            edges.append(
                {
                    "name": name,
                    "declared": dependency is not None,
                    "classification": "declared"
                    if dependency
                    else "unresolved; system/API-set status unverified",
                }
            )
            if dependency:
                queue.append(dependency)
        output["pe"].append(
            dict(parsed, path=str(path), edges=edges, inspection=result)
        )
    return all(
        p.get("code") == 0
        and not p.get("timeout")
        and not p.get("truncated")
        and not p.get("reader_incomplete")
        for p in output["probes"]
    ) and all(p["stdout"].strip() == commit[0] for p in output["probes"][:2])


def main():
    env = dict(os.environ)
    output = {
        "checkout_head": env.get("VOICE_DIAGNOSTIC_HEAD"),
        "checkout_tree": env.get("VOICE_DIAGNOSTIC_TREE"),
        "schema": 1,
        "github": {
            key: env.get(key)
            for key in (
                "GITHUB_SHA",
                "GITHUB_EVENT_NAME",
                "GITHUB_REF",
                "GITHUB_RUN_ID",
            )
        },
    }
    success = False
    try:
        if not all(
            re.fullmatch(r"[0-9a-f]{40}", output.get(key) or "")
            for key in ("checkout_head", "checkout_tree")
        ):
            raise ValueError("actual checkout HEAD/tree receipt unavailable")
        if output["checkout_head"] != env.get("GITHUB_SHA"):
            raise ValueError("checkout HEAD differs from dispatched GitHub SHA")
        success = diagnose(json.loads(Path(sys.argv[1]).read_text()), env, output)
    except Exception as error:
        output["incomplete"] = str(error)
    output["success"] = success
    destination = Path(env["TEST_UNDECLARED_OUTPUTS_DIR"])
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "voice-runtime-diagnostic.json").write_text(
        json.dumps(output, indent=2), encoding="utf-8"
    )
    print(
        json.dumps(
            {
                "success": success,
                "incomplete": output.get("incomplete"),
                "probe_statuses": [
                    p.get("code_u32_hex", "spawn error")
                    for p in output.get("probes", [])
                ],
            }
        )
    )
    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())
