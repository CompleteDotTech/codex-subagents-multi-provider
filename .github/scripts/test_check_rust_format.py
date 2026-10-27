from pathlib import Path
import os
import re
import runpy
import subprocess
import tempfile
import tomllib
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
                "id": "age-vendor",
                "name": "age",
                "manifest_path": str(workspace / "third-party/age/Cargo.toml"),
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
        self.assertNotIn("age", commands[0])
        self.assertIn("imports_granularity=Item", commands[0])
        self.assertIn("aws-config", commands[1])
        self.assertIn("imports_granularity=Preserve", commands[1])
        self.assertIn("--check", commands[1])
        self.assertNotIn("external", commands[0])
        standalone = formatting.standalone_format_commands(formatting.WORKSPACE)
        self.assertTrue(standalone)
        self.assertEqual(standalone[0][0], "rustfmt")
        self.assertIn("imports_granularity=Preserve,skip_children=true", standalone[0])
        self.assertIn("--check", standalone[0])

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
        self.assertTrue(
            all(
                "--check" in command
                for command in formatting.standalone_format_commands(
                    formatting.WORKSPACE
                )
            )
        )

    def test_repository_format_entrypoint_uses_verified_policy_in_both_modes(self):
        driver = runpy.run_path(str(formatting.ROOT / "scripts/format.py"))
        for check in (True, False):
            group = driver["rust_formatter_group"](check=check)
            self.assertGreaterEqual(len(group.commands), 2)
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
            self.assertFalse(
                any(
                    "third-party/age" in arg.replace("\\", "/")
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

    def test_either_vendor_can_fail_before_metadata_or_a_writer(self):
        for index in (0, 1):
            effects = [None] * index + [subprocess.CalledProcessError(1, "integrity")]
            with patch.object(formatting.subprocess, "run", side_effect=effects) as run:
                with patch.object(formatting.subprocess, "check_output") as metadata:
                    with self.assertRaises(subprocess.CalledProcessError):
                        formatting.main()
            self.assertEqual(run.call_count, index + 1)
            metadata.assert_not_called()

    def test_age_cannot_rejoin_the_application_workspace(self):
        workspace, metadata = self.metadata()
        metadata["workspace_members"].append("age-vendor")
        with self.assertRaisesRegex(ValueError, "outside the application workspace"):
            formatting.format_commands(metadata, workspace)

    def test_production_manifest_keeps_standalone_test_resolution_outside_app(self):
        app = tomllib.loads((formatting.WORKSPACE / "Cargo.toml").read_text())
        age = tomllib.loads(
            (formatting.WORKSPACE / "third-party/age/Cargo.toml").read_text()
        )
        self.assertIn("third-party/age", app["workspace"]["exclude"])
        self.assertNotIn("third-party/age", app["workspace"]["members"])
        self.assertEqual(age["workspace"], {})
        self.assertEqual(app["patch"]["crates-io"]["age"], {"path": "third-party/age"})
        pin = app["patch"]["crates-io"]["age-core"]
        self.assertEqual(pin["rev"], "b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f")
        self.assertEqual(age["dependencies"]["age-core"]["rev"], pin["rev"])
        self.assertEqual(age["dependencies"]["age-core"]["git"], pin["git"])

    def test_both_vendor_aliases_are_rejected_by_direct_rustfmt_discovery(self):
        driver = runpy.run_path(str(formatting.ROOT / "scripts/format.py"))
        group = driver["rust_formatter_group"]
        for vendor in ("aws-config", "age"):
            for hardlink in (False, True):
                with tempfile.TemporaryDirectory(prefix="format aliases ") as temporary:
                    root = Path(temporary)
                    protected = root / "codex-rs/third-party" / vendor
                    protected.mkdir(parents=True)
                    source = protected / "original.rs"
                    source.write_text("pub fn original() {}\n")
                    alias = root / "codex-rs/alias.rs"
                    try:
                        if hardlink:
                            os.link(source, alias)
                        else:
                            alias.symlink_to(source)
                    except OSError as error:
                        self.skipTest(f"alias not available: {error}")
                    with patch.dict(group.__globals__, REPO_ROOT=root):
                        with patch.object(
                            subprocess,
                            "check_output",
                            return_value=b"codex-rs/alias.rs\0",
                        ):
                            with self.assertRaisesRegex(
                                ValueError, "aliases protected"
                            ):
                                group(check=False)

    def test_spelling_vendor_exclusions_have_both_exact_guards_first(self):
        workflow = (formatting.ROOT / ".github/workflows/codespell.yml").read_text()
        check = workflow.index("      - name: Codespell\n")
        for verifier in formatting.VERIFIED_VENDORS.values():
            self.assertLess(
                workflow.index(f"python3 codex-rs/third-party/{verifier}"), check
            )
        configuration = (formatting.ROOT / ".codespellrc").read_text()
        self.assertIn("./codex-rs/third-party/age/*", configuration)
        self.assertNotIn("./codex-rs/third-party/*", configuration)

    def test_orphan_and_unknown_vendor_sources_stay_in_the_strict_writer(self):
        driver = runpy.run_path(str(formatting.ROOT / "scripts/format.py"))
        group = driver["rust_formatter_group"]
        with tempfile.TemporaryDirectory(prefix="format orphan coverage ") as temporary:
            root = Path(temporary)
            sources = ["codex-rs/orphan.rs", "codex-rs/third-party/unknown/source.rs"]
            for name in sources:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("pub fn orphan() {}\n")
            with patch.dict(group.__globals__, REPO_ROOT=root):
                with patch.object(
                    subprocess, "check_output", return_value="\0".join(sources).encode()
                ):
                    commands = group(check=False).commands
            args = commands[1].args
            self.assertIn("orphan.rs", args)
            self.assertIn(os.path.join("third-party", "unknown", "source.rs"), args)
            self.assertIn("imports_granularity=Item,skip_children=true", args)


if __name__ == "__main__":
    unittest.main()
