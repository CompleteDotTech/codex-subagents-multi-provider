import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import windows_runtime_diagnostic as diagnostic


class DiagnosticTests(unittest.TestCase):
    def test_exe_dll_and_deferred_imports(self):
        text = """File Type: EXECUTABLE IMAGE
            8664 machine (x64)
          Image has the following dependencies:
            glib-2.0-0.dll
          Image has the following delay load dependencies:
            optional.dll
          Summary
        """
        parsed = diagnostic.parse_pe(text)
        self.assertEqual(parsed["image_kind"], "exe")
        self.assertEqual(parsed["imports"], ["glib-2.0-0.dll"])
        self.assertEqual(parsed["delay_imports"], ["optional.dll"])
        self.assertEqual(
            diagnostic.parse_pe(text.replace("EXECUTABLE IMAGE", "DLL"))["image_kind"],
            "dll",
        )
        with self.assertRaises(ValueError):
            diagnostic.parse_pe("truncated")

    def test_manifest_only_and_traversal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            physical = root / "file with spaces.exe"
            physical.write_bytes(b"PE")
            manifest = root / "MANIFEST"
            manifest.write_text(f"_main/helper.exe {physical}\n")
            env = {"RUNFILES_MANIFEST_FILE": str(manifest)}
            self.assertEqual(
                diagnostic.resolve("_main/helper.exe", env), physical.resolve()
            )
            for key in ["../file", "/absolute", "C:/file", "_main/../file"]:
                with self.assertRaises(ValueError):
                    diagnostic.resolve(key, env)
            with self.assertRaises(ValueError):
                diagnostic.resolve("_main/missing", env)

    def test_ambiguous_runtime(self):
        with self.assertRaises(ValueError):
            diagnostic.inventory([Path("a/B.dll"), Path("b/b.DLL")])

    def test_fixture_matches_installed_client_copy_policy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.exe"
            source.write_bytes(b"same helper")
            runtime = root / "native" / "bin"
            runtime.mkdir(parents=True)
            library = runtime / "voice.dll"
            library.write_bytes(b"library")
            marker = runtime.parent / "runtime.json"
            marker.write_text("{}")
            installed = diagnostic.fixture(root / "fixture", source, [library, marker])
            self.assertEqual(diagnostic.digest(installed), diagnostic.digest(source))
            self.assertTrue((installed.parent / "voice.dll").exists())
            self.assertFalse((installed.parent / "runtime.json").exists())
            self.assertTrue((root / "fixture/bin/codex.exe").exists())

    def test_file_size_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "large.exe"
            path.write_bytes(b"12345")
            with patch.object(diagnostic, "MAX_FILE", 4), self.assertRaises(ValueError):
                diagnostic.digest(path)

    def test_process_contract_status_and_spawn_error(self):
        import io

        child = unittest.mock.Mock(stdout=io.BytesIO(b"commit"), stderr=io.BytesIO(b""))
        child.wait.return_value = 0xC0000135
        env = dict(os.environ)
        with patch.object(diagnostic.subprocess, "Popen", return_value=child) as spawn:
            result = diagnostic.run(
                ["helper.exe", "--build-commit"], env, Path("unrelated")
            )
            self.assertIs(spawn.call_args.kwargs["env"], env)
            self.assertEqual(spawn.call_args.kwargs["cwd"], Path("unrelated"))
            self.assertEqual(result["signed_code"], -1073741515)
            self.assertEqual(result["code_u32_hex"], "0xc0000135")
        with patch.object(
            diagnostic.subprocess, "Popen", side_effect=OSError("missing")
        ):
            self.assertIn("spawn_error", diagnostic.run(["helper.exe"], env, Path(".")))

    def test_timeout_and_bounded_output(self):
        import io

        child = unittest.mock.Mock(
            stdout=io.BytesIO(b"x" * 100), stderr=io.BytesIO(b"")
        )
        child.wait.side_effect = [
            diagnostic.subprocess.TimeoutExpired("helper", 10),
            -1,
        ]
        with patch.object(diagnostic.subprocess, "Popen", return_value=child):
            result = diagnostic.run(["helper"], {}, Path("."), 4)
        child.kill.assert_called_once()
        self.assertTrue(result["timeout"])
        self.assertTrue(result["truncated"])
        self.assertEqual(result["stdout"], "xxxx")

    def test_diagnose_preserves_source_and_copy_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = {}
            for name in ["helper.exe", "unit.exe", "dumpbin.exe", "status"]:
                path = root / name
                path.write_bytes(b"PE")
                files[name] = path
            files["status"].write_text("STABLE_GIT_COMMIT " + "a" * 40 + "\n")
            config = dict(
                helper="helper.exe",
                unit="unit.exe",
                dumpbin="dumpbin.exe",
                status="status",
                runtime=[],
                tool_environment={"TOOL_ONLY": "yes"},
            )
            env = {"TEST_TMPDIR": str(root)}
            calls = []

            def runner(command, environment, cwd, *args):
                calls.append((command, dict(environment), cwd))
                text = (
                    "a" * 40
                    if "--build-commit" in command
                    else "8664 machine (x64)\nFile Type: EXECUTABLE IMAGE"
                )
                return {"code": 0, "stdout": text, "stderr": "", "truncated": False}

            with (
                patch.object(
                    diagnostic, "resolve", side_effect=lambda key, _: files[key]
                ),
                patch.object(diagnostic, "run", side_effect=runner),
            ):
                output = {}
                self.assertTrue(diagnostic.diagnose(config, env, output))
            self.assertEqual(calls[0][1:], calls[1][1:])
            self.assertEqual(calls[0][1], env)
            self.assertNotIn("TOOL_ONLY", calls[0][1])
            self.assertIn("TOOL_ONLY", calls[-1][1])
            self.assertEqual(output["helper_sha256"], output["installed_sha256"])


if __name__ == "__main__":
    unittest.main()
