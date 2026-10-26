import contextlib
import io
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

import verify_cargo_workspace_manifests as policy


class VerifiedVendorManifestPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cargo policy ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.cargo = self.root / "codex-rs"
        shutil.copytree(policy.ROOT / "codex-rs/third-party", self.cargo / "third-party")
        (self.cargo / "auth").mkdir()
        (self.cargo / "Cargo.toml").write_text(
            '[workspace]\nmembers=["auth","third-party/aws-config"]\n'
            '[workspace.package]\nversion="0.164.0"\nedition="2024"\nlicense="Apache-2.0"\n'
            '[workspace.dependencies]\naws-config={path="third-party/aws-config"}\n'
        )
        self.auth = self.cargo / "auth/Cargo.toml"
        self.auth.write_text(
            '[package]\nname="codex-auth"\nversion.workspace=true\nedition.workspace=true\nlicense.workspace=true\n'
            '[lints]\nworkspace=true\n'
            '[dependencies]\naws-config={workspace=true,features=["credentials-login"]}\n'
        )
        self.addCleanup(patch.stopall)
        patch.object(policy, "ROOT", self.root).start()
        patch.object(policy, "CARGO_RS_ROOT", self.cargo).start()
        patch.object(policy, "MANIFEST_FEATURE_EXCEPTIONS", {}).start()

    def run_policy(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = policy.main()
        return result, output.getvalue()

    def test_verified_external_manifest_and_auth_features_are_accepted(self):
        self.assertEqual(self.run_policy(), (0, ""))
        verified = policy.verified_vendor_manifests()
        self.assertNotIn("aws-config", policy.workspace_package_names(verified))
        self.assertFalse(policy.is_internal_dependency(
            self.auth, "aws-config", {"path": "../third-party/aws-config"},
            policy.workspace_package_names(verified), verified,
        ))

    def test_vendor_source_manifest_and_fixture_tampering_fail_closed(self):
        for name in ("Cargo.toml", "src/login/dpop.rs", "test-data/ecs-tests.json", "rustfmt.toml", "BUILD.bazel"):
            with self.subTest(file=name):
                path = self.cargo / "third-party/aws-config" / name
                original = path.read_bytes()
                path.write_bytes(original + b"\n# unauthorized change\n")
                try:
                    status, output = self.run_policy()
                    self.assertEqual(status, 1)
                    self.assertIn("integrity check failed", output)
                    self.assertIn("checksum", output)
                finally:
                    path.write_bytes(original)

    def test_missing_guard_cannot_enable_the_vendor_policy(self):
        (self.cargo / "third-party/verify_aws_config.py").unlink()
        status, output = self.run_policy()
        self.assertEqual(status, 1)
        self.assertIn("integrity check failed", output)

    def test_verified_external_dependency_does_not_allow_first_party_optional_toggle(self):
        self.auth.write_text(self.auth.read_text().replace('features=["credentials-login"]', 'optional=true'))
        status, output = self.run_policy()
        self.assertEqual(status, 1)
        self.assertIn("new optional dependencies are not allowed", output)

    def test_unverified_third_party_package_is_still_first_party_policy(self):
        other = self.cargo / "third-party/unverified"
        other.mkdir()
        (other / "Cargo.toml").write_text('[package]\nname="unverified"\nversion="1.0.0"\nedition="2021"\n')
        status, output = self.run_policy()
        self.assertEqual(status, 1)
        self.assertIn("third-party/unverified/Cargo.toml", output)
        self.assertIn("version.workspace", output)
        self.assertIn("workspace = true", output)

    def test_first_party_feature_and_metadata_policy_remains_enforced(self):
        with self.auth.open("a") as target:
            target.write('\n[features]\nnew-feature=[]\n')
        status, output = self.run_policy()
        self.assertEqual(status, 1)
        self.assertIn("new workspace crate features are not allowed", output)
        self.auth.write_text(self.auth.read_text().replace('version.workspace=true', 'version="1.0.0"'))
        self.assertIn("version.workspace", self.run_policy()[1])

    def test_first_party_dependency_features_are_not_reclassified(self):
        other = self.cargo / "other"
        other.mkdir()
        (other / "Cargo.toml").write_text(self.auth.read_text().replace('name="codex-auth"', 'name="codex-other"'))
        with self.auth.open("a") as target:
            target.write('codex-other={path="../other",features=["unapproved"]}\n')
        status, output = self.run_policy()
        self.assertEqual(status, 1)
        self.assertIn("new workspace crate feature activations are not allowed", output)


if __name__ == "__main__":
    unittest.main()
