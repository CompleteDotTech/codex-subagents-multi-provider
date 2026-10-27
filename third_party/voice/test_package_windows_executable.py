"""Declared payload copying, collision protection, and voice consumer contracts."""

import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import package_windows_executable as packaging

DLL_NAMES = """gio-2.0-0.dll glib-2.0-0.dll gmodule-2.0-0.dll gobject-2.0-0.dll
    gstapp-1.0-0.dll gstapp.dll gstaudio-1.0-0.dll gstaudioconvert.dll
    gstaudioresample.dll gstbase-1.0-0.dll gstcoreelements.dll gstnet-1.0-0.dll
    gstopus.dll gstpbutils-1.0-0.dll gstreamer-1.0-0.dll gstrtp-1.0-0.dll
    gstrtp.dll gstrtpmanager.dll gsttag-1.0-0.dll gstvideo-1.0-0.dll intl-8.dll
    libffi-8.dll opus.dll pcre2-8.dll z.dll""".split()


class PackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="voice package ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.inputs = self.root / "declared inputs"
        self.inputs.mkdir()
        self.executable = self.inputs / "codex-voice-host.exe"
        self.executable.write_bytes(b"unchanged executable\x00\xff")
        self.dlls = []
        for name in DLL_NAMES:
            path = self.inputs / name
            path.write_bytes((name + "\x00payload").encode())
            self.dlls.append(path)
        self.config = self.config_for("helper")

    def config_for(self, name, executable=None):
        executable = executable or self.executable
        directory = self.root / name
        return {
            "directory": str(directory),
            "receipt": str(directory / "packaging.json"),
            "executable": {
                "source": str(executable),
                "output": str(directory / executable.name),
            },
            "dlls": [
                {"source": str(path), "output": str(directory / path.name)}
                for path in self.dlls
            ],
        }

    def assert_no_payload_written(self, config):
        directory = Path(config["directory"])
        self.assertFalse(any(directory.glob("*.exe")))
        self.assertFalse(any(directory.glob("*.dll")))

    def test_two_unique_packages_keep_all_25_dlls_and_identical_bytes(self):
        unit = self.inputs / "codex-voice-host-bin-unit-tests-bin.exe"
        unit.write_bytes(b"unchanged unit executable")
        for name, executable in [
            ("helper package", self.executable),
            ("unit package", unit),
        ]:
            before = {path: path.read_bytes() for path in [executable, *self.dlls]}
            config = self.config_for(name, executable)
            packaging.package(config)
            directory = Path(config["directory"])
            self.assertEqual(len(config["dlls"]), 25)
            self.assertEqual(
                {path.name for path in directory.iterdir()},
                {executable.name, "packaging.json", *DLL_NAMES},
            )
            receipt = json.loads(
                (directory / "packaging.json").read_text(encoding="utf-8")
            )
            self.assertEqual(len(receipt["dlls"]), 25)
            for source, value in before.items():
                self.assertEqual(source.read_bytes(), value)
                self.assertEqual((directory / source.name).read_bytes(), value)
            self.assertEqual(
                receipt["executable"]["source_sha256"],
                packaging.digest(executable),
            )
            self.assertEqual(
                receipt["executable"]["source_sha256"],
                receipt["executable"]["output_sha256"],
            )
            self.assertTrue(
                all(
                    row["source_sha256"] == row["output_sha256"]
                    for row in receipt["dlls"]
                )
            )

    def test_case_insensitive_collision_rejected_before_writes_even_same_bytes(self):
        other = self.inputs / "GLIB-2.0-0.DLL"
        other.write_bytes(self.dlls[1].read_bytes())
        self.config["dlls"].append(
            {
                "source": str(other),
                "output": str(Path(self.config["directory"]) / other.name),
            }
        )
        with self.assertRaisesRegex(ValueError, "collision"):
            packaging.package(self.config)
        self.assert_no_payload_written(self.config)

    def test_duplicate_exact_input_mapping_is_not_silently_overwritten(self):
        self.config["dlls"].append(dict(self.config["dlls"][0]))
        with self.assertRaisesRegex(ValueError, "collision"):
            packaging.package(self.config)
        self.assert_no_payload_written(self.config)

    def test_preexisting_exe_dll_or_receipt_never_overwritten(self):
        for target in [self.executable.name, DLL_NAMES[-1], "packaging.json"]:
            with self.subTest(target=target):
                config = self.config_for("preexisting-" + target)
                directory = Path(config["directory"])
                directory.mkdir()
                existing = directory / target
                existing.write_bytes(b"preserve me")
                with self.assertRaisesRegex(ValueError, "already exists"):
                    packaging.package(config)
                self.assertEqual(list(directory.iterdir()), [existing])
                self.assertEqual(existing.read_bytes(), b"preserve me")

    def test_preexisting_case_alias_is_not_overwritten_on_case_sensitive_exec_hosts(
        self,
    ):
        directory = Path(self.config["directory"])
        directory.mkdir()
        existing = directory / DLL_NAMES[-1].upper()
        existing.write_bytes(b"preserve case alias")
        with self.assertRaisesRegex(ValueError, "already exists"):
            packaging.package(self.config)
        self.assertEqual(list(directory.iterdir()), [existing])
        self.assertEqual(existing.read_bytes(), b"preserve case alias")

    def test_missing_late_input_fails_before_first_output(self):
        self.dlls[-1].unlink()
        with self.assertRaisesRegex(ValueError, "input is missing"):
            packaging.package(self.config)
        self.assert_no_payload_written(self.config)

    def test_no_dlls_or_escaped_output_or_source_destination_alias(self):
        empty = copy.deepcopy(self.config)
        empty["dlls"] = []
        with self.assertRaisesRegex(ValueError, "must not be empty"):
            packaging.package(empty)
        escaped = copy.deepcopy(self.config)
        escaped["dlls"][-1]["output"] = str(self.root / "outside" / DLL_NAMES[-1])
        with self.assertRaisesRegex(ValueError, "escapes"):
            packaging.package(escaped)
        alias = copy.deepcopy(self.config)
        alias["directory"] = str(self.inputs)
        alias["executable"]["output"] = str(self.executable)
        with self.assertRaisesRegex(ValueError, "must differ"):
            packaging.package(alias)
        self.assert_no_payload_written(self.config)

    def test_changed_copy_bytes_fail_instead_of_publishing_success_receipt(self):
        def corrupt(_source, output, _length):
            output.write(b"changed bytes")

        with patch.object(packaging.shutil, "copyfileobj", side_effect=corrupt):
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                packaging.package(self.config)
        self.assertFalse(Path(self.config["receipt"]).exists())


class ConsumerContractTests(unittest.TestCase):
    def test_raw_compilation_and_packaged_execution_routes_remain_distinct(self):
        root = Path(__file__).resolve().parents[2]
        source = (root / "defs.bzl").read_text(encoding="utf-8")
        self.assertIn("binary_windows_runtime = {}", source)
        self.assertIn(
            'binary_kwargs = {"binary_name": binary} if runtime else {}', source
        )
        self.assertIn('crate = ":" + raw_binary', source)
        self.assertIn('"//conditions:default": ":" + raw_binary', source)
        self.assertIn('"//conditions:default": exposed_unit', source)
        self.assertIn("test_bin = exposed_unit", source)
        self.assertIn(
            'cargo_env["CARGO_BIN_EXE_" + binary] = "$(rlocationpath :%s)" % binary',
            source,
        )
        voice = (root / "codex-rs/voice-host/BUILD.bazel").read_text(encoding="utf-8")
        self.assertIn(
            'unit = ":codex-voice-host-bin-unit-tests-bin__windows-runtime"', voice
        )
        windows = (root / ".github/workflows/rust-release-windows.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "cp bazel-bin/codex-rs/voice-host/codex-voice-host__windows-runtime/codex-voice-host.exe",
            windows,
        )
        macos = (root / ".github/workflows/rust-release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            'cp bazel-bin/codex-rs/voice-host/codex-voice-host "$output/codex-voice-host"',
            macos,
        )


if __name__ == "__main__":
    unittest.main()
