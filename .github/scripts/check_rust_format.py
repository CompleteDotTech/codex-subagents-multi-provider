#!/usr/bin/env python3
"""Check strict first-party formatting and the integrity-pinned AWS vendor style."""

import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = ROOT / "codex-rs"


def format_commands(metadata: dict, workspace: Path) -> list[list[str]]:
    members = set(metadata["workspace_members"])
    packages = [package for package in metadata["packages"] if package["id"] in members]
    vendor_manifest = (workspace / "third-party/aws-config/Cargo.toml").resolve()
    vendor = [package for package in packages if Path(package["manifest_path"]).resolve() == vendor_manifest]
    if len(vendor) != 1 or vendor[0]["name"] != "aws-config":
        raise ValueError("expected exactly the verified aws-config workspace member")
    first_party = sorted(package["name"] for package in packages if package["id"] != vendor[0]["id"])
    if not first_party:
        raise ValueError("no first-party packages selected for strict formatting")
    return [
        ["cargo", "fmt", "--package", *first_party, "--", "--config", "imports_granularity=Item", "--check"],
        ["cargo", "fmt", "--package", "aws-config", "--", "--config", "imports_granularity=Preserve", "--check"],
    ]


def main() -> None:
    # Integrity failure stops before the vendor can receive its separate policy.
    subprocess.run([sys.executable, str(WORKSPACE / "third-party/verify_aws_config.py")], check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], cwd=WORKSPACE
    ))
    for command in format_commands(metadata, WORKSPACE):
        subprocess.run(command, cwd=WORKSPACE, check=True)


if __name__ == "__main__":
    main()
