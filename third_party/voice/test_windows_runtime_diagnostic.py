import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import windows_runtime_diagnostic as diagnostic
import package_windows_executable as packaging


class DiagnosticTests(unittest.TestCase):
    def packaged_fixture(self, root):
        files = {}
        runtime = root / "native" / "bin"
        runtime.mkdir(parents=True)
        dll = runtime / "voice.dll"
        dll.write_bytes(b"declared DLL")
        files["runtime-dll"] = dll
        packages = {}
        for name in ["helper", "unit"]:
            source = root / (name + "-raw") / (name + ".exe")
            source.parent.mkdir()
            source.write_bytes(b"unchanged PE")
            directory = root / (name + "-package")
            packaging.package(
                {
                    "directory": str(directory),
                    "executable": {
                        "source": str(source),
                        "output": str(directory / source.name),
                    },
                    "dlls": [{"source": str(dll), "output": str(directory / dll.name)}],
                    "receipt": str(directory / "packaging.json"),
                }
            )
            files[name + ".exe"] = directory / source.name
            files[name + "-raw"] = source
            files[name + "-receipt"] = directory / "packaging.json"
            files[name + "-dll"] = directory / dll.name
            packages[name] = {
                "raw": name + "-raw",
                "receipt": name + "-receipt",
                "dlls": [name + "-dll"],
            }
        for name in ["dumpbin.exe", "status"]:
            files[name] = root / name
            files[name].write_bytes(b"PE")
        files["status"].write_text("STABLE_GIT_COMMIT " + "a" * 40 + "\n")
        config = dict(
            helper="helper.exe",
            unit="unit.exe",
            dumpbin="dumpbin.exe",
            status="status",
            runtime=["runtime-dll"],
            packages=packages,
            tool_environment={"TOOL_ONLY": "yes"},
        )
        return files, config

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
            files, config = self.packaged_fixture(root)
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
            self.assertTrue(
                all(value["valid"] for value in output["packaging"].values())
            )
            self.assertEqual(output["packaging"]["helper"]["declared_dll_count"], 1)

    def test_package_check_rejects_missing_modified_dll_and_executable_bytes(self):
        for change in ["missing", "DLL bytes", "EXE bytes", "receipt"]:
            with (
                self.subTest(change=change),
                tempfile.TemporaryDirectory() as directory,
            ):
                files, config = self.packaged_fixture(Path(directory))
                if change == "missing":
                    files["helper-dll"].unlink()
                elif change == "DLL bytes":
                    files["helper-dll"].write_bytes(b"changed")
                elif change == "EXE bytes":
                    files["helper.exe"].write_bytes(b"changed")
                else:
                    files["helper-receipt"].write_text("{}")
                with patch.object(
                    diagnostic, "resolve", side_effect=lambda key, _: files[key]
                ):
                    with self.assertRaises((ValueError, KeyError)):
                        diagnostic.package_adjacency(
                            files["helper.exe"],
                            config["packages"]["helper"],
                            {"voice.dll": files["runtime-dll"]},
                            {},
                        )

    def test_failed_package_check_retains_all_original_startup_and_pe_metrics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files, config = self.packaged_fixture(root)
            files["helper-dll"].write_bytes(b"changed package DLL")

            def runner(command, environment, cwd, *args):
                if "--build-commit" in command:
                    installed = "installed-fixture" in command[0]
                    return {
                        "code": 0 if installed else 0xC0000135,
                        "stdout": "a" * 40 if installed else "",
                        "stderr": "",
                    }
                if "--list" in command:
                    return {"code": 0xC0000135, "stdout": "", "stderr": ""}
                return {
                    "code": 0,
                    "stdout": "8664 machine (x64)\nFile Type: EXECUTABLE IMAGE",
                    "stderr": "",
                }

            with (
                patch.object(
                    diagnostic, "resolve", side_effect=lambda key, _: files[key]
                ),
                patch.object(diagnostic, "run", side_effect=runner),
            ):
                output = {}
                self.assertFalse(
                    diagnostic.diagnose(config, {"TEST_TMPDIR": str(root)}, output)
                )
            self.assertEqual(
                [row["code"] for row in output["probes"]], [0xC0000135, 0, 0xC0000135]
            )
            self.assertEqual(len(output["pe"]), 3)
            self.assertEqual(output["helper_sha256"], output["installed_sha256"])
            self.assertFalse(output["packaging"]["helper"]["valid"])


if __name__ == "__main__":
    unittest.main()
