from pathlib import Path
import re
import runpy
import subprocess
import unittest
from unittest.mock import patch

import check_rust_format as formatting


class RustFormatPolicyTests(unittest.TestCase):
    def test_ci_workflows_use_required_integrity_aware_format_check(self):
        for filename in ("rust-ci.yml", "rust-ci-full.yml"):
            with self.subTest(workflow=filename):
                source = (formatting.ROOT / ".github/workflows" / filename).read_text(
                    encoding="utf-8"
                )
                general = re.search(
                    r"^  general:\n(.*?)(?=^  [a-zA-Z_][\w-]*:|\Z)",
                    source,
                    re.M | re.S,
                ).group(1)
                self.assertIn("        working-directory: codex-rs\n", general)
                step = re.search(
                    r"^      - name: cargo fmt\n(.*?)(?=^      - |\Z)",
                    general,
                    re.M | re.S,
                ).group(1)
                self.assertEqual(
                    step.strip(),
                    "run: python3 ../.github/scripts/check_rust_format.py",
                )
                # The helper owns package selection and fail-closed integrity;
                # direct whole-workspace formatting must not reappear in CI.
                self.assertNotRegex(source, r"\brun:\s*cargo\s+fmt\b")

    def metadata(self):
        workspace = Path("workspace").resolve()
        packages = [
            {
                "id": "first",
                "name": "codex-example",
                "manifest_path": str(workspace / "example/Cargo.toml"),
            },
            {
                "id": "vendor",
                "name": "aws-config",
                "manifest_path": str(workspace / "third-party/aws-config/Cargo.toml"),
            },
            {
                "id": "registry",
                "name": "external",
                "manifest_path": "/registry/external/Cargo.toml",
            },
        ]
        return workspace, {
            "packages": packages,
            "workspace_members": ["first", "vendor"],
        }

    def test_only_exact_vendor_gets_separate_style_policy(self):
        workspace, metadata = self.metadata()
        commands = formatting.format_commands(metadata, workspace)
        self.assertIn("codex-example", commands[0])
        self.assertNotIn("aws-config", commands[0])
        self.assertIn("imports_granularity=Item", commands[0])
        self.assertIn("aws-config", commands[1])
        self.assertIn("imports_granularity=Preserve", commands[1])
        self.assertIn("--check", commands[1])
        self.assertNotIn("external", commands[0])

    def test_missing_or_relocated_vendor_fails_closed(self):
        workspace, metadata = self.metadata()
        metadata["packages"][1]["manifest_path"] = str(workspace / "other/Cargo.toml")
        with self.assertRaises(ValueError):
            formatting.format_commands(metadata, workspace)

    def test_all_other_workspace_packages_remain_in_strict_pass(self):
        workspace, metadata = self.metadata()
        metadata["packages"].append(
            {
                "id": "other",
                "name": "unverified-vendor",
                "manifest_path": str(workspace / "third-party/other/Cargo.toml"),
            }
        )
        metadata["workspace_members"].append("other")
        self.assertIn(
            "unverified-vendor", formatting.format_commands(metadata, workspace)[0]
        )

    def test_integrity_failure_stops_before_metadata_or_formatting(self):
        with patch.object(
            formatting.subprocess,
            "run",
            side_effect=subprocess.CalledProcessError(1, "integrity"),
        ) as run:
            with patch.object(formatting.subprocess, "check_output") as metadata:
                with self.assertRaises(subprocess.CalledProcessError):
                    formatting.main()
        self.assertEqual(run.call_count, 1)
        metadata.assert_not_called()

    def test_write_mode_only_formats_first_party(self):
        workspace, metadata = self.metadata()
        commands = formatting.format_commands(metadata, workspace, check=False)
        self.assertNotIn("--check", commands[0])
        self.assertIn("imports_granularity=Item", commands[0])
        self.assertIn("--check", commands[1])
        self.assertIn("imports_granularity=Preserve", commands[1])

    def test_repository_format_entrypoint_uses_verified_policy_in_both_modes(self):
        driver = runpy.run_path(str(formatting.ROOT / "scripts/format.py"))
        for check in (True, False):
            group = driver["rust_formatter_group"](check=check)
            self.assertEqual(len(group.commands), 2)
            self.assertEqual(group.commands[1].args[0], "rustfmt")
            self.assertIn(
                "imports_granularity=Item,skip_children=true", group.commands[1].args
            )
            self.assertEqual("--check" in group.commands[1].args, check)
            self.assertFalse(
                any(
                    "third-party/aws-config" in arg.replace("\\", "/")
                    for arg in group.commands[1].args
                )
            )
            command = group.commands[0]
            self.assertEqual(
                command.args[:2],
                (
                    formatting.sys.executable,
                    str(formatting.ROOT / ".github/scripts/check_rust_format.py"),
                ),
            )
            self.assertEqual(command.args[2:], () if check else ("--write",))
            self.assertEqual(command.cwd, formatting.WORKSPACE)


if __name__ == "__main__":
    unittest.main()
