import configparser
import re
import unittest
from pathlib import Path
from unittest.mock import patch

import verify_cargo_workspace_manifests as manifests

ROOT = Path(__file__).resolve().parents[2]


class CheckoutAndLintPolicyTest(unittest.TestCase):
    def test_windows_jobs_enable_long_paths_before_checkout(self):
        # Include both reusable nextest jobs: ARM64 shards and x64 archives
        # each check out the repository independently.
        windows_jobs = {
            "bazel.yml": [
                "test-windows-shard",
                "test-windows-native-main",
                "clippy",
                "verify-release-build",
            ],
            "rust-ci.yml": ["argument_comment_lint_prebuilt"],
            "rust-ci-full.yml": ["argument_comment_lint_prebuilt", "lint_build"],
            "rust-ci-full-nextest-platform.yml": ["archive", "shard"],
        }
        for filename, jobs in windows_jobs.items():
            source = (ROOT / ".github/workflows" / filename).read_text(encoding="utf-8")
            for job in jobs:
                with self.subTest(workflow=filename, job=job):
                    block = re.search(
                        r"^  " + re.escape(job) + r":\n(.*?)(?=^  [a-zA-Z_][\w-]*:|\Z)",
                        source,
                        re.M | re.S,
                    )
                    self.assertIsNotNone(block)
                    steps = block.group(1).split("    steps:\n", 1)[1]
                    before_checkout = steps.split("uses: actions/checkout@", 1)[0]
                    self.assertIn("if: runner.os == 'Windows'", before_checkout)
                    self.assertIn("shell: pwsh", before_checkout)
                    self.assertIn(
                        "run: git config --global core.longpaths true", before_checkout
                    )

    def test_code_mode_cannot_reintroduce_feature_exception(self):
        manifest = {
            "package": {
                "name": "codex-code-mode",
                **{k: {"workspace": True} for k in manifests.WORKSPACE_PACKAGE_FIELDS},
            },
            "lints": {"workspace": True},
            "features": {"sandbox": ["v8/v8_enable_sandbox"]},
        }
        with patch.object(manifests, "load_manifest", return_value=manifest):
            errors = manifests.manifest_errors(
                manifests.CARGO_RS_ROOT / "code-mode/Cargo.toml",
                set(),
                set(),
                set(),
                set(),
            )
        self.assertIn(
            "remove `[features]`; new workspace crate features are not allowed", errors
        )

    def test_stale_feature_exceptions_remain_errors(self):
        errors = {}
        with patch.object(
            manifests, "MANIFEST_FEATURE_EXCEPTIONS", {"obsolete/Cargo.toml": {}}
        ):
            manifests.add_unused_exception_errors(errors, set(), set(), set())
        self.assertIn(
            "remove the stale `[features]` exception", errors["obsolete/Cargo.toml"][0]
        )

    def test_codespell_exceptions_require_exact_technical_tokens(self):
        config = configparser.ConfigParser(interpolation=None)
        config.read(ROOT / ".codespellrc", encoding="utf-8")
        ignored = re.compile(config["codespell"]["ignore-regex"])
        self.assertEqual(ignored.sub("", "OFlags::WRONLY"), "OFlags::")
        self.assertEqual(ignored.sub("", r"\oint"), "")
        self.assertEqual(ignored.sub("", '"oint" => "∮"'), ' => "∮"')
        for prose in ["oint", '"oint"', "WRONLYsuffix", r"\ointment", "WRONGLY"]:
            self.assertEqual(ignored.sub("", prose), prose)


if __name__ == "__main__":
    unittest.main()
