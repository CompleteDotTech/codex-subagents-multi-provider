from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

import check_rust_format as formatting


class RustFormatPolicyTests(unittest.TestCase):
    def metadata(self):
        workspace = Path("workspace").resolve()
        packages = [
            {"id": "first", "name": "codex-example", "manifest_path": str(workspace / "example/Cargo.toml")},
            {"id": "vendor", "name": "aws-config", "manifest_path": str(workspace / "third-party/aws-config/Cargo.toml")},
            {"id": "registry", "name": "external", "manifest_path": "/registry/external/Cargo.toml"},
        ]
        return workspace, {"packages": packages, "workspace_members": ["first", "vendor"]}

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
        metadata["packages"].append({"id": "other", "name": "unverified-vendor", "manifest_path": str(workspace / "third-party/other/Cargo.toml")})
        metadata["workspace_members"].append("other")
        self.assertIn("unverified-vendor", formatting.format_commands(metadata, workspace)[0])

    def test_integrity_failure_stops_before_metadata_or_formatting(self):
        with patch.object(formatting.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "integrity")) as run:
            with patch.object(formatting.subprocess, "check_output") as metadata:
                with self.assertRaises(subprocess.CalledProcessError):
                    formatting.main()
        self.assertEqual(run.call_count, 1)
        metadata.assert_not_called()


if __name__ == "__main__":
    unittest.main()
