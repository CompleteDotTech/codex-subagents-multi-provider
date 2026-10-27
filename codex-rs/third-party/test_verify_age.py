import contextlib
import ast
import hashlib
import io
import json
from pathlib import Path
import shutil
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import verify_age as integrity


class AgeIntegrityTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="age integrity ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        shutil.copytree(integrity.ROOT / "age", self.root / "age")
        for name in ("age.upstream.json", "age.patched.json", "age.patch"):
            shutil.copyfile(integrity.ROOT / name, self.root / name)

    def verify(self, **kwargs):
        with contextlib.redirect_stdout(io.StringIO()):
            integrity.verify(self.root, **kwargs)

    def update_metadata(self, filename, edit):
        path = self.root / filename
        data = json.loads(path.read_text())
        edit(data)
        path.write_text(json.dumps(data))

    def test_exact_snapshot_and_reverse_patch_verify_without_network_or_tools(self):
        self.verify()

    def test_source_manifest_license_fixture_style_and_build_tampering_fail(self):
        for name in (
            "src/native/scrypt.rs",
            "Cargo.toml",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "tests/testdata/testkit/scrypt",
            "rustfmt.toml",
            "BUILD.bazel",
            "Cargo.lock",
        ):
            with self.subTest(name=name):
                path = self.root / "age" / name
                original = path.read_bytes()
                path.write_bytes(original + b"\nunauthorized change\n")
                try:
                    with self.assertRaisesRegex(ValueError, "checksum"):
                        self.verify()
                finally:
                    path.write_bytes(original)

    def test_unknown_and_missing_files_fail_closed(self):
        unexpected = self.root / "age/extra.rs"
        unexpected.write_text("// unreviewed\n")
        with self.assertRaisesRegex(ValueError, "file"):
            self.verify()
        unexpected.unlink()
        (self.root / "age/LICENSE-MIT").unlink()
        with self.assertRaisesRegex(ValueError, "file"):
            self.verify()

    def test_broad_patch_scope_cannot_be_added_only_through_metadata(self):
        self.update_metadata(
            "age.patched.json",
            lambda data: data["files"].update({"src/lib.rs": "0" * 64}),
        )
        with self.assertRaisesRegex(ValueError, "patch scope"):
            self.verify()

    def test_manifest_normalization_is_independently_checked(self):
        manifest = self.root / "age/Cargo.toml"
        manifest.write_text(
            manifest.read_text().replace('version = "0.12.1"', 'version = "0.12.2"')
        )
        digest = hashlib.sha256(manifest.read_bytes()).hexdigest()
        self.update_metadata(
            "age.patched.json",
            lambda data: data["files"].update({"Cargo.toml": digest}),
        )
        with self.assertRaisesRegex(ValueError, "normalization"):
            self.verify()

    def test_patch_hash_and_reconstruction_are_both_required(self):
        path = self.root / "age.patch"
        path.write_bytes(path.read_bytes().replace(b"@@ -", b"@@ invalid-", 1))
        with self.assertRaisesRegex(ValueError, "patch checksum"):
            self.verify()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        self.update_metadata(
            "age.patched.json", lambda data: data.update(patch_sha256=digest)
        )
        with self.assertRaisesRegex(ValueError, "patch hunk"):
            self.verify()

    def test_upstream_revision_and_archive_checksum_are_fixed(self):
        archive = self.root / "invalid.tar"
        archive.write_bytes(b"not the approved public archive")
        with self.assertRaisesRegex(ValueError, "archive checksum"):
            self.verify(archive=archive)
        self.update_metadata(
            "age.upstream.json", lambda data: data.update(revision="unreviewed")
        )
        with self.assertRaisesRegex(ValueError, "revision"):
            self.verify()

    def test_vendor_symlink_is_rejected_even_if_target_bytes_match(self):
        source = self.root / "age/README.md"
        target = self.root / "copied-readme"
        source.replace(target)
        try:
            source.symlink_to(target)
        except OSError as error:
            self.skipTest(f"symlink not available: {error}")
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.verify()

    def test_vendor_hardlink_is_rejected_even_if_bytes_match(self):
        source = self.root / "age/README.md"
        target = self.root / "hardlink"
        try:
            target.hardlink_to(source)
        except OSError as error:
            self.skipTest(f"hardlink not available: {error}")
        with self.assertRaisesRegex(ValueError, "hardlink"):
            self.verify()

    def test_vendor_directory_junction_is_not_a_valid_snapshot_root(self):
        with patch.object(Path, "is_junction", return_value=True, create=True):
            with self.assertRaisesRegex(ValueError, "junction"):
                self.verify()

    def test_explicit_integration_feature_closures_preserve_upstream_requirements(self):
        import re

        manifest = tomllib.loads((self.root / "age/Cargo.toml").read_text())
        source = (self.root / "age/BUILD.bazel").read_text()

        def closure(features):
            selected = set(features)
            pending = list(features)
            while pending:
                name = pending.pop()
                for child in manifest["features"].get(name, []):
                    if child not in selected:
                        selected.add(child)
                        pending.append(child)
            return selected

        for variable, test_name in (
            ("_SSH_FEATURES", "test_vectors"),
            ("_ASYNC_FEATURES", "testkit"),
        ):
            required = next(
                test["required-features"]
                for test in manifest["test"]
                if test["name"] == test_name
            )
            actual = ast.literal_eval(
                re.search(rf"{variable} = (\[[\s\S]*?\])", source)[1]
            )
            intended = required + (
                ["cli-common"] if test_name == "test_vectors" else []
            )
            self.assertTrue(set(required) <= set(actual))
            self.assertEqual(set(actual), closure(intended) | {"default"})
            if test_name == "test_vectors":
                # Cargo's required-features alone would select a ZERO-test binary.
                # Bind the intended feature set to the real pinned test function gate.
                vectors = (self.root / "age/tests/test_vectors.rs").read_text()
                gated_tests = re.findall(
                    r'#\[test\]\s*#\[cfg\(feature = "([^"]+)"\)\]\s*fn (\w+)', vectors
                )
                active = [name for feature, name in gated_tests if feature in actual]
                self.assertIn("age_test_vectors", active)
                self.assertGreater(len(active), 0)
                module = (integrity.ROOT.parents[1] / "MODULE.bazel").read_text()
                annotations = re.findall(r"crate\.annotation\(([\s\S]*?)\n\)", module)
                annotation = next(
                    block
                    for block in annotations
                    if 'crate = "age"' in block and '"age_ssh_test_crates"' in block
                )
                selected = ast.literal_eval(
                    re.search(r"crate_features = (\[[\s\S]*?\])", annotation)[1]
                )
                self.assertEqual(set(selected), set(intended))
                rules = re.findall(r"rust_(?:library|test)\(([\s\S]*?)\n\)", source)
                for target in ("age-ssh-test-lib", "age-test-vectors-ssh-bin"):
                    rule = next(
                        block for block in rules if f'name = "{target}"' in block
                    )
                    self.assertIn("crate_features = _SSH_FEATURES", rule)
                    self.assertIn("deps = ssh_deps(", rule)
        self.assertEqual(manifest["features"]["default"], [])

    def test_exact_test_dependency_closures_exclude_benchmark_only_deps(self):
        import re

        source = (self.root / "age/BUILD.bazel").read_text()
        expected = {
            "age-unit-tests-bin": ("unit", ["proptest-1.11.0"]),
            "age-test-vectors-ssh-bin": ("ssh", []),
            "age-testkit-armor-async-bin": (
                "async",
                [
                    "flate2-1.1.9",
                    "hex-0.4.3",
                    "test-case-3.3.1",
                    "tokio-1.52.3",
                ],
            ),
        }
        locked = tomllib.loads((self.root / "age/Cargo.lock").read_text())["package"]
        identity_to_name = {
            f"{row['name']}-{row['version']}": row["name"] for row in locked
        }
        manifest = tomllib.loads((self.root / "age/Cargo.toml").read_text())
        normal_names = {
            spec.get("package", name) if isinstance(spec, dict) else name
            for name, spec in manifest["dependencies"].items()
        }
        # i18n stays source-required, supplied ONCE by every normal hub closure.
        self.assertIn("i18n-embed", normal_names)
        self.assertTrue(
            {"fluent-system", "desktop-requester"}
            <= set(manifest["dev-dependencies"]["i18n-embed"]["features"])
        )
        rules = re.findall(r"rust_test\(([\s\S]*?)\n\)", source)
        for target, (hub, labels) in expected.items():
            rule = next(block for block in rules if f'name = "{target}"' in block)
            self.assertIn(f"{hub}_deps(normal = True)", rule)
            actual = re.findall(r'"@([^"]+)//:([^"]+)"', rule)
            self.assertEqual(
                set(actual), {(f"age_{hub}_test_crates", label) for label in labels}
            )
            self.assertEqual(len(actual), len(set(actual)))
            self.assertTrue(set(labels) <= identity_to_name.keys())
            # Concatenating a normal label duplicates it before Rust's provider
            # filtering; the public wrapper forwards that list into both attrs.
            self.assertTrue(
                {identity_to_name[label] for _, label in actual}.isdisjoint(
                    normal_names
                )
            )
            self.assertNotIn("normal_dev", rule)
            for name in ("criterion", "pprof", "inferno", "futures-test"):
                self.assertNotIn(f"//:{name}-", rule)
        manifest = tomllib.loads((self.root / "age/Cargo.toml").read_text())
        self.assertIn("criterion", manifest["dev-dependencies"])
        self.assertTrue((self.root / "age/benches").is_dir())

    def test_standalone_lock_cannot_be_dropped_from_integrity_scope(self):
        self.update_metadata(
            "age.patched.json", lambda data: data["integration"].pop("Cargo.lock")
        )
        with self.assertRaisesRegex(ValueError, "integration scope"):
            self.verify()

    def test_archive_traversal_is_rejected_before_extraction(self):
        import tarfile

        archive = self.root / "traversal.tar"
        with tarfile.open(archive, "w") as target:
            member = tarfile.TarInfo("../outside")
            member.size = 1
            target.addfile(member, io.BytesIO(b"x"))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.update_metadata(
            "age.upstream.json", lambda data: data.update(archive_sha256=digest)
        )
        with patch.object(integrity, "ARCHIVE_SHA256", digest):
            with self.assertRaisesRegex(ValueError, "traversal"):
                self.verify(archive=archive)
        self.assertFalse((self.root.parent / "outside").exists())


if __name__ == "__main__":
    unittest.main()
