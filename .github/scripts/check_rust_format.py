#!/usr/bin/env python3
"""Check strict first-party formatting and exact integrity-pinned vendor styles."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = ROOT / "codex-rs"
VERIFIED_VENDORS = {
    "aws-config": "verify_aws_config.py",
    "age": "verify_age.py",
}


def format_commands(
    metadata: dict, workspace: Path, *, check: bool = True
) -> list[list[str]]:
    members = set(metadata["workspace_members"])
    packages = [package for package in metadata["packages"] if package["id"] in members]
    age_manifest = (workspace / "third-party/age/Cargo.toml").resolve()
    if any(
        Path(package["manifest_path"]).resolve() == age_manifest for package in packages
    ):
        raise ValueError("age must remain outside the application workspace")
    vendor_ids = set()
    for name in ("aws-config",):
        manifest = (workspace / "third-party" / name / "Cargo.toml").resolve()
        vendor = [
            package
            for package in packages
            if Path(package["manifest_path"]).resolve() == manifest
        ]
        if len(vendor) != 1 or vendor[0]["name"] != name:
            raise ValueError(f"expected exactly the verified {name} workspace member")
        vendor_ids.add(vendor[0]["id"])
    first_party = sorted(
        package["name"] for package in packages if package["id"] not in vendor_ids
    )
    if not first_party:
        raise ValueError("no first-party packages selected for strict formatting")
    suffix = ["--check"] if check else []
    commands = [
        [
            "cargo",
            "fmt",
            "--package",
            *first_party,
            "--",
            "--config",
            "imports_granularity=Item",
            *suffix,
        ],
    ]
    for name in ("aws-config",):
        commands.append(
            [
                "cargo",
                "fmt",
                "--package",
                name,
                "--",
                "--config",
                "imports_granularity=Preserve",
                "--check",
            ]
        )
    return commands


def standalone_format_commands(workspace: Path) -> list[list[str]]:
    """Check the verified standalone age tree without Cargo/dev graph resolution."""
    vendor = workspace / "third-party/age"
    sources = sorted(str(path) for path in vendor.rglob("*.rs"))
    if not sources:
        raise ValueError("verified age Rust sources are missing")
    prefix = [
        "rustfmt",
        "--edition",
        "2024",
        "--config-path",
        str(vendor / "rustfmt.toml"),
        "--config",
        "imports_granularity=Preserve,skip_children=true",
        "--check",
        "--",
    ]
    commands = []
    batch = prefix.copy()
    for source in sources:
        if (
            len(subprocess.list2cmdline([*prefix, source]).encode("utf-16-le")) // 2 + 1
            > 16_000
        ):
            raise ValueError("age formatter path exceeds Windows command-line bound")
        if (
            len(subprocess.list2cmdline([*batch, source]).encode("utf-16-le")) // 2 + 1
            > 16_000
        ):
            commands.append(batch)
            batch = prefix.copy()
        batch.append(source)
    commands.append(batch)
    return commands


def main(*, check: bool = True) -> None:
    # Integrity failure stops before the vendor can receive its separate policy.
    for verifier in VERIFIED_VENDORS.values():
        subprocess.run(
            [sys.executable, str(WORKSPACE / "third-party" / verifier)],
            check=True,
        )
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
            cwd=WORKSPACE,
        )
    )
    for command in [
        *format_commands(metadata, WORKSPACE, check=check),
        *standalone_format_commands(WORKSPACE),
    ]:
        subprocess.run(command, cwd=WORKSPACE, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write",
        action="store_true",
        help="format first-party Rust; verify vendor bytes and style",
    )
    main(check=not parser.parse_args().write)
