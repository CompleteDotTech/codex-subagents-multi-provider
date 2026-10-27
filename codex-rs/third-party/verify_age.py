#!/usr/bin/env python3
"""Verify the exact licensed age snapshot, normalization, and reviewed patch offline."""

import argparse
import copy
import difflib
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import tarfile
import tomllib


REVISION = "b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f"
ARCHIVE_SHA256 = "3c04247d64d18b001abcf25b77ae0e3a5e1eb4ecd1d1b1622223c681a87acce0"
ROOT = Path(__file__).resolve().parent


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def checksum(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def is_link(path: Path) -> bool:
    return path.is_symlink() or getattr(path, "is_junction", lambda: False)()


def normalized_manifest(original: bytes, workspace: bytes) -> dict:
    """Resolve only upstream inheritance, preserving its dependency/feature policy."""
    result = tomllib.loads(original.decode())
    inherited = tomllib.loads(workspace.decode())["workspace"]
    for key, value in result["package"].items():
        if isinstance(value, dict) and value.get("workspace"):
            result["package"][key] = copy.deepcopy(inherited["package"][key])
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        for name, value in result.get(section, {}).items():
            if not isinstance(value, dict) or not value.get("workspace"):
                continue
            expanded = copy.deepcopy(inherited["dependencies"][name])
            if isinstance(expanded, str):
                expanded = {"version": expanded}
            additions = {key: item for key, item in value.items() if key != "workspace"}
            features = expanded.get("features", []) + additions.pop("features", [])
            expanded.update(additions)
            if features:
                expanded["features"] = list(dict.fromkeys(features))
            if name == "age-core":
                expanded.pop("path", None)
                expanded.update(git="https://github.com/str4d/rage", rev=REVISION)
            result[section][name] = expanded
    result["lints"] = copy.deepcopy(inherited["lints"])
    # A local dependency must not unify its dev/test features into app metadata.
    result["workspace"] = {}
    return result


def original_from_patch(current: str, patch_lines: list[str]) -> str:
    """Reverse checked unified hunks, then independently reconstruct the exact diff."""
    lines = current.splitlines(True)
    output = []
    cursor = 0
    index = 0
    while index < len(patch_lines):
        header = re.fullmatch(
            r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@.*\n", patch_lines[index]
        )
        require(header is not None, "invalid patch hunk")
        start = int(header[3]) - 1
        if int(header[4] or "1") == 0:
            start += 1
        require(cursor <= start <= len(lines), "patch hunk order")
        output.extend(lines[cursor:start])
        cursor = start
        index += 1
        old_count = new_count = 0
        while index < len(patch_lines) and not patch_lines[index].startswith("@@ "):
            line = patch_lines[index]
            require(line[:1] in (" ", "+", "-"), "invalid patch line")
            if line[0] in " +":
                require(
                    cursor < len(lines) and lines[cursor] == line[1:],
                    "patch content mismatch",
                )
                cursor += 1
                new_count += 1
            if line[0] in " -":
                output.append(line[1:])
                old_count += 1
            index += 1
        require(old_count == int(header[2] or "1"), "patch old count")
        require(new_count == int(header[4] or "1"), "patch new count")
    output.extend(lines[cursor:])
    return "".join(output)


def verify(root: Path = ROOT, archive: Path | None = None) -> None:
    vendor = root / "age"
    upstream = json.loads((root / "age.upstream.json").read_text(encoding="utf-8"))
    patched = json.loads((root / "age.patched.json").read_text(encoding="utf-8"))
    require(upstream["revision"] == REVISION, "unexpected upstream revision")
    require(
        upstream["archive_sha256"] == ARCHIVE_SHA256,
        "unexpected upstream archive identity",
    )
    require(len(upstream["files"]) == 174, "unexpected upstream file scope")
    require(
        set(patched["files"]) == {"Cargo.toml", "src/native/scrypt.rs"},
        "unexpected patch scope",
    )
    require(
        set(patched["extras"]) == {"Cargo.toml.orig", "UPSTREAM-WORKSPACE.toml"},
        "unexpected provenance scope",
    )
    require(set(patched["style"]) == {"rustfmt.toml"}, "unexpected vendor style scope")
    require(
        "BUILD.bazel" in patched["integration"]
        and set(patched["integration"]) <= {"BUILD.bazel", "Cargo.lock"},
        "unexpected integration scope",
    )
    expected = (
        upstream["files"]
        | patched["files"]
        | patched["extras"]
        | patched["style"]
        | patched["integration"]
    )
    require(not is_link(vendor), "vendor root is a symlink or junction")
    actual = set()
    for path in vendor.rglob("*"):
        require(not is_link(path), f"vendor symlink or junction: {path.name}")
        require(
            path.resolve().is_relative_to(vendor.resolve()), "vendor path escapes tree"
        )
        if path.is_file():
            require(path.stat().st_nlink == 1, f"vendor hardlink: {path.name}")
            actual.add(path.relative_to(vendor).as_posix())
    require(actual == expected.keys(), "unexpected or missing vendor files")
    for name, digest in expected.items():
        key = PurePosixPath(name)
        require(
            not key.is_absolute() and ".." not in key.parts, "invalid metadata path"
        )
        require(
            checksum((vendor / name).read_bytes()) == digest, f"file checksum: {name}"
        )
    require(
        "Permission is hereby granted" in (vendor / "LICENSE-MIT").read_text(),
        "missing MIT license",
    )
    require(
        "Apache License" in (vendor / "LICENSE-APACHE").read_text(),
        "missing Apache license",
    )
    original_manifest = (vendor / "Cargo.toml.orig").read_bytes()
    workspace = (vendor / "UPSTREAM-WORKSPACE.toml").read_bytes()
    require(
        checksum(original_manifest) == upstream["files"]["Cargo.toml"],
        "original manifest checksum",
    )
    require(
        checksum(workspace) == upstream["workspace_manifest_sha256"],
        "workspace manifest checksum",
    )
    require(
        tomllib.loads((vendor / "Cargo.toml").read_text())
        == normalized_manifest(original_manifest, workspace),
        "manifest normalization differs",
    )

    patch = (root / "age.patch").read_bytes()
    require(checksum(patch) == patched["patch_sha256"], "patch checksum")
    sections = {}
    current = None
    patch_lines = patch.decode().splitlines(True)
    index = 0
    while index < len(patch_lines):
        if patch_lines[index].startswith("--- a/age/"):
            current = patch_lines[index].removeprefix("--- a/age/").rstrip("\n")
            require(index + 1 < len(patch_lines), "patch missing destination")
            require(
                current in patched["files"] and current not in sections,
                "unexpected patch filename",
            )
            require(
                patch_lines[index + 1] == f"+++ b/age/{current}\n",
                "patch destination mismatch",
            )
            sections[current] = []
            index += 2
            continue
        require(current is not None, "patch missing file header")
        sections[current].append(patch_lines[index])
        index += 1
    require(sections.keys() == patched["files"].keys(), "patch file set")
    reconstructed = ""
    originals = {}
    for name, hunks in sections.items():
        updated = (vendor / name).read_text(encoding="utf-8")
        original = original_from_patch(updated, hunks)
        require(
            checksum(original.encode()) == upstream["files"][name],
            f"reconstructed upstream checksum: {name}",
        )
        originals[name] = original.encode()
        reconstructed += "".join(
            difflib.unified_diff(
                original.splitlines(True),
                updated.splitlines(True),
                fromfile=f"a/age/{name}",
                tofile=f"b/age/{name}",
                n=3,
            )
        )
    require(reconstructed.encode() == patch, "patch does not reconstruct")

    if archive is not None:
        data = archive.read_bytes()
        require(checksum(data) == ARCHIVE_SHA256, "upstream archive checksum")
        with tarfile.open(fileobj=io.BytesIO(data)) as source:
            entries = {}
            for member in source.getmembers():
                name = PurePosixPath(member.name)
                require(
                    not name.is_absolute() and ".." not in name.parts,
                    "archive traversal",
                )
                require(
                    member.isdir() or member.isfile(), "archive link or special file"
                )
                if member.isfile():
                    require(member.name not in entries, "duplicate archive entry")
                    entries[member.name] = member
            expected_archive = {
                (
                    "age/" + name
                    if name not in {"LICENSE-MIT", "LICENSE-APACHE"}
                    else name
                ): digest
                for name, digest in upstream["files"].items()
            }
            expected_archive["Cargo.toml"] = upstream["workspace_manifest_sha256"]
            require(
                entries.keys() == expected_archive.keys(), "unexpected archive file set"
            )
            for name, digest in expected_archive.items():
                require(
                    checksum(source.extractfile(entries[name]).read()) == digest,
                    f"archive file checksum: {name}",
                )
    print(
        "Verified 174 upstream files, 2 patched files, exact provenance, style, integration and reconstructed patch"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    try:
        verify(archive=args.archive)
    except (ValueError, KeyError, OSError, tomllib.TOMLDecodeError) as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
