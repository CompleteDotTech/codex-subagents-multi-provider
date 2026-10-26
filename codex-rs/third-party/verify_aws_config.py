#!/usr/bin/env python3
"""Verify the licensed AWS snapshot, immutable fixtures, and reviewed patch.

The default check is offline. --archive additionally reconstructs the exact
unified patch from the checksum-pinned crates.io archive, without extraction.
"""

import argparse
import difflib
import hashlib
import io
import json
from pathlib import Path
import tarfile


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    vendor = root / "aws-config"
    upstream = json.loads((root / "aws-config.upstream.json").read_text(encoding="utf-8"))
    patch_metadata = json.loads((root / "aws-config.patched.json").read_text(encoding="utf-8"))
    patched = patch_metadata["files"]
    style = patch_metadata["style"]
    integration = patch_metadata["integration"]
    fixtures = json.loads((root / "aws-config.test-data.json").read_text(encoding="utf-8"))["files"]
    require(set(patched) == {"Cargo.toml", "Cargo.toml.orig", "src/login/dpop.rs", "src/lib.rs"}, "unexpected patch scope")
    require(set(style) == {"rustfmt.toml"}, "unexpected vendor style scope")
    require(set(integration) == {"BUILD.bazel"}, "unexpected vendor integration scope")
    expected = upstream["files"] | patched | fixtures | style | integration
    actual = {path.relative_to(vendor).as_posix() for path in vendor.rglob("*") if path.is_file()}
    require(actual == expected.keys(), "unexpected or missing vendor files")
    for name, checksum in expected.items():
        require(hashlib.sha256((vendor / name).read_bytes()).hexdigest() == checksum, f"file checksum: {name}")
    require("Apache License" in (vendor / "LICENSE").read_text(encoding="utf-8"), "missing upstream license")
    if args.archive:
        archive = args.archive.read_bytes()
        require(hashlib.sha256(archive).hexdigest() == upstream["sha256"], "upstream archive checksum")
        diff = []
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            for name, checksum in upstream["files"].items():
                original = source.extractfile(f"aws-config-1.8.12/{name}").read()
                require(hashlib.sha256(original).hexdigest() == checksum, f"archive file checksum: {name}")
                if name in patched:
                    diff.extend(difflib.unified_diff(
                        original.decode().splitlines(True),
                        (vendor / name).read_text(encoding="utf-8").splitlines(True),
                        fromfile=f"a/{name}", tofile=f"b/{name}", n=0,
                    ))
        require("".join(diff) == (root / "aws-config.patch").read_text(encoding="utf-8"), "patch does not reconstruct")
    print(f"Verified {len(upstream['files'])} upstream files, {len(patched)} patched files, and {len(fixtures)} fixtures")


if __name__ == "__main__":
    main()
