"""Execute the actual rendered launcher with harmless fixtures under Windows CMD."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


@unittest.skipUnless(os.name == "nt", "Windows CMD bootstrap regression")
class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="voice launcher spaces ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cmd = Path(os.environ["SystemRoot"]) / "System32/cmd.exe"
        self.python = Path(sys.executable).resolve(strict=True)
        self.keys = {
            "__PYTHON__": "_diag_python/" + self.python.name,
            "__DRIVER__": "_main/driver.py",
            "__CONFIG__": "_main/config.json",
        }
        self.launcher = self.root / "diagnostic launcher.bat"
        template = (
            Path(__file__)
            .with_name("windows_runtime_diagnostic.bat.tpl")
            .read_bytes()
            .decode()
        )
        for token, key in self.keys.items():
            template = template.replace(token, key)
        self.launcher.write_bytes(template.encode())
        self.output = self.root / "observed.json"
        self.driver = self.root / "harmless driver.py"
        self.driver.write_text(
            "import json,os,sys\n"
            "from pathlib import Path\n"
            f"sys.path.insert(0, {str(Path(__file__).parent)!r})\n"
            "from windows_runtime_diagnostic import resolve\n"
            "config=json.loads(Path(sys.argv[1]).read_text())\n"
            "assert resolve('_main/config.json',os.environ)==Path(sys.argv[1]).resolve()\n"
            "Path(config['output']).write_text(json.dumps({k:os.environ.get(k) for k in "
            "['RUNFILES_MANIFEST_FILE','RUNFILES_DIR','TEST_SRCDIR','VOICE_DIAGNOSTIC_RUNFILES_REASON']}))\n"
        )
        self.config = self.root / "harmless config.json"
        self.config.write_text(json.dumps({"output": str(self.output)}))
        self.env = dict(os.environ)
        for key in ["RUNFILES_MANIFEST_FILE", "RUNFILES_DIR", "TEST_SRCDIR"]:
            self.env.pop(key, None)

    def manifest(self, path, missing=None, nonexistent=None):
        physical = {
            "__PYTHON__": self.python,
            "__DRIVER__": self.driver,
            "__CONFIG__": self.config,
        }
        path.write_text(
            "".join(
                f"{key} {self.root / 'absent' if token == nonexistent else physical[token]}\n"
                for token, key in self.keys.items()
                if token != missing
            )
        )

    def run_launcher(self):
        result = subprocess.run(
            f'"{self.cmd}" /d /s /c ""{self.launcher}""',
            cwd=self.root,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=15,
        )
        return result

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue(self.output.exists(), result.stdout)
        return json.loads(self.output.read_text())

    def test_manifest_only_with_spaces(self):
        path = self.root / "manifest with spaces"
        self.manifest(path)
        self.env["RUNFILES_MANIFEST_FILE"] = str(path)
        observed = self.assert_success(self.run_launcher())
        self.assertEqual(observed["RUNFILES_MANIFEST_FILE"], str(path))
        self.assertFalse(observed["RUNFILES_DIR"])
        self.assertEqual(
            observed["VOICE_DIAGNOSTIC_RUNFILES_REASON"],
            "existing_environment_manifest",
        )

    def test_directory_only(self):
        root = self.root / "runfiles directory"
        (root / "_main").mkdir(parents=True)
        # A directory junction preserves the concrete interpreter's support tree.
        junction = root / "_diag_python"
        command = (
            f'"{self.cmd}" /d /s /c "mklink /J "{junction}" "{self.python.parent}""'
        )
        made = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
        )
        self.assertEqual(made.returncode, 0, made.stdout)
        self.addCleanup(lambda: junction.rmdir() if junction.exists() else None)
        (root / self.keys["__DRIVER__"]).write_bytes(self.driver.read_bytes())
        (root / self.keys["__CONFIG__"]).write_bytes(self.config.read_bytes())
        self.env["TEST_SRCDIR"] = str(root)
        self.env["RUNFILES_MANIFEST_FILE"] = str(self.root / "bogus manifest")
        observed = self.assert_success(self.run_launcher())
        self.assertFalse(observed["RUNFILES_MANIFEST_FILE"])
        self.assertEqual(observed["RUNFILES_DIR"], str(root))
        self.assertEqual(observed["TEST_SRCDIR"], str(root))

    def test_absent_or_bogus_environment_adjacent_manifest_fallback(self):
        manifest = Path(str(self.launcher) + ".runfiles_manifest")
        self.manifest(manifest)
        for bogus in [False, True]:
            with self.subTest(bogus=bogus):
                if bogus:
                    self.env["RUNFILES_MANIFEST_FILE"] = str(
                        self.root / "missing manifest"
                    )
                    self.env["RUNFILES_DIR"] = str(self.root / "missing root")
                observed = self.assert_success(self.run_launcher())
                self.assertEqual(observed["RUNFILES_MANIFEST_FILE"], str(manifest))
                self.assertEqual(
                    observed["VOICE_DIAGNOSTIC_RUNFILES_REASON"],
                    "existing_adjacent_manifest",
                )

    def test_manifest_inside_environment_runfiles_directory(self):
        root = self.root / "manifest only runfiles directory"
        root.mkdir()
        manifest = root / "MANIFEST"
        self.manifest(manifest)
        self.env["RUNFILES_DIR"] = str(root)
        observed = self.assert_success(self.run_launcher())
        self.assertEqual(Path(observed["RUNFILES_MANIFEST_FILE"]), manifest)
        self.assertEqual(
            observed["VOICE_DIAGNOSTIC_RUNFILES_REASON"], "existing_directory_manifest"
        )

    def test_missing_and_nonexistent_roles_fail_clearly(self):
        manifest = self.root / "MANIFEST"
        self.env["RUNFILES_MANIFEST_FILE"] = str(manifest)
        for token, role in [
            ("__PYTHON__", "python"),
            ("__DRIVER__", "driver"),
            ("__CONFIG__", "config"),
        ]:
            for missing_file in [False, True]:
                with self.subTest(role=role, missing_file=missing_file):
                    self.manifest(
                        manifest,
                        missing=None if missing_file else token,
                        nonexistent=token if missing_file else None,
                    )
                    result = self.run_launcher()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(role + " runfile", result.stdout)
                    self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
