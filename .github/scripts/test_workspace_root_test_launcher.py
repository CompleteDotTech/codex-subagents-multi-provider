"""Execute launchers expanded by the real rule with lightweight Bazel API stand-ins."""

import ast
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest


ROOT = Path(__file__).resolve().parents[2]


class Runfiles:
    def __init__(self, files=()):
        self.files = list(files)

    def merge(self, other):
        return Runfiles(self.files + other.files)


class Target:
    def __init__(self, label):
        self.label = label

    def __getitem__(self, _provider):
        return SimpleNamespace(default_runfiles=Runfiles(), files=SimpleNamespace(to_list=lambda: []))


class LabelDict(dict):
    def keys(self):
        return list(super().keys())


def expand_launcher(output, windows, custom_marker):
    # Execute the actual rule/helper definitions, not a parallel implementation
    # of their substitutions. No Bazel analysis, compiler, or build is launched.
    source = ast.parse((ROOT / "defs.bzl").read_text())
    names = {
        "_workspace_root_test_impl", "_bash_runfile_env_exports",
        "_windows_runfile_env_exports", "_runfile_logical_path",
        "_bash_workspace_root_setup", "_windows_workspace_root_setup",
    }
    selected = ast.Module(body=[node for node in source.body if isinstance(node, ast.FunctionDef) and node.name in names], type_ignores=[])
    namespace = {
        "DefaultInfo": lambda **kwargs: SimpleNamespace(**kwargs),
        "RunEnvironmentInfo": lambda **kwargs: SimpleNamespace(**kwargs),
        "platform_common": SimpleNamespace(ConstraintValueInfo=object()),
        "depset": list,
    }
    exec(compile(selected, str(ROOT / "defs.bzl"), "exec"), namespace)
    marker = SimpleNamespace(short_path="codex-rs/third-party/aws-config/Cargo.toml") if custom_marker else None
    workspace_marker = SimpleNamespace(short_path="codex-rs/utils/cargo-bin/repo_root.marker")
    test_bin = SimpleNamespace(short_path="bin/observer.exe" if windows else "bin/observer")

    def expand_template(*, template, output, substitutions, **_kwargs):
        text = template.read_bytes().decode()
        for key, value in substitutions.items():
            text = text.replace(key, value)
        if "__WORKING_DIRECTORY_MARKER__" in text:
            raise AssertionError("unexpanded working-directory marker")
        output.write_bytes(text.encode())

    ctx = SimpleNamespace(
        label=SimpleNamespace(name="launcher"),
        actions=SimpleNamespace(declare_file=lambda _: output, expand_template=expand_template),
        executable=SimpleNamespace(test_bin=test_bin),
        file=SimpleNamespace(
            workspace_root_marker=workspace_marker, working_directory_marker=marker,
            _windows_launcher_template=ROOT / "workspace_root_test_launcher.bat.tpl",
            _bash_launcher_template=ROOT / "workspace_root_test_launcher.sh.tpl",
        ),
        attr=SimpleNamespace(
            _windows_constraint=Target("windows"), data=[], runfile_env=LabelDict(), env={},
            env_inherit=[], test_threads=0, chdir_workspace_root=True,
            test_bin=Target("test"), workspace_root_marker=Target("root"),
        ),
        target_platform_has_constraint=lambda _: windows,
        runfiles=lambda files: Runfiles(files),
        expand_location=lambda value, _: value,
    )
    results = namespace["_workspace_root_test_impl"](ctx)
    if marker is not None and marker not in results[0].runfiles.files:
        raise AssertionError("custom marker missing from declared runfiles")
    return test_bin.short_path, workspace_marker.short_path, marker.short_path if marker else ""


class WorkspaceRootLauncherTests(unittest.TestCase):
    def run_launcher(self, *, custom, manifest, missing=False):
        windows = os.name == "nt"
        with tempfile.TemporaryDirectory(prefix="workspace launcher ") as temporary:
            root = Path(temporary)
            launcher = root / ("launch.bat" if windows else "launch.sh")
            binary_key, workspace_key, custom_key = expand_launcher(launcher, windows, custom)
            physical = root / ("physical files with spaces" if manifest else "runfiles tree/workspace")
            workspace = physical / "codex-rs"
            crate = workspace / "third-party/aws-config"
            (workspace / "utils/cargo-bin").mkdir(parents=True)
            (workspace / "utils/cargo-bin/repo_root.marker").write_text("root")
            crate.mkdir(parents=True)
            if not missing:
                (crate / "Cargo.toml").write_text("[package]")
            expected = crate if custom else workspace
            (expected / "test-data").mkdir()
            (expected / "test-data/fixture.txt").write_text("relative fixture opened")
            binary = physical / binary_key
            binary.parent.mkdir(parents=True, exist_ok=True)
            # Use the actual host Python executable, including on native Windows.
            # Its DLL and stdlib remain discoverable from the existing installation.
            shutil.copyfile(sys.executable, binary)
            binary.chmod(0o755)
            probe = root / "observe.py"
            result = root / "result.json"
            probe.write_text(
                "import json,os,sys\nfrom pathlib import Path\n"
                "Path(sys.argv[1]).write_text(json.dumps({'cwd':str(Path.cwd()),"
                "'insta':os.environ.get('INSTA_WORKSPACE_ROOT'),"
                "'fixture':Path('test-data/fixture.txt').read_text()}))\n"
            )
            env = os.environ.copy()
            for name in list(env):
                if name.startswith(("RUNFILES", "TEST_", "RULES_RUST_TEST_")) or name in ("TESTBRIDGE_TEST_ONLY", "CODEX_BAZEL_TEST_SKIP_FILTERS"):
                    env.pop(name)
            env["TEST_WORKSPACE"] = "workspace"
            env["PYTHONHOME"] = sys.base_prefix
            env["PATH"] = str(Path(sys.executable).parent) + os.pathsep + env.get("PATH", "")
            if manifest:
                entries = [(binary_key, binary), (workspace_key, physical / workspace_key)]
                if custom:
                    entries.append((custom_key, physical / custom_key))
                manifest_path = root / "runfiles manifest"
                manifest_path.write_text("".join(f"workspace/{key} {path}\n" for key, path in entries))
                env["RUNFILES_MANIFEST_FILE"] = str(manifest_path)
            else:
                env["RUNFILES_DIR"] = str(root / "runfiles tree")
            command = (
                'cmd.exe /d /s /c "' + subprocess.list2cmdline([str(launcher), str(probe), str(result)]) + '"'
                if windows else ["/bin/bash", str(launcher), str(probe), str(result)]
            )
            process = subprocess.run(command, env=env, text=True, capture_output=True, timeout=20)
            if missing:
                self.assertNotEqual(process.returncode, 0, process.stdout + process.stderr)
                self.assertFalse(result.exists(), "test binary ran despite missing cwd marker")
                self.assertIn("cargo.toml", process.stderr.lower() + process.stdout.lower())
            else:
                self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
                observed = json.loads(result.read_text())
                self.assertEqual(Path(observed["cwd"]), expected)
                self.assertEqual(Path(observed["insta"]), workspace)
                self.assertEqual(observed["fixture"], "relative fixture opened")

    def test_default_workspace_cwd_tree_and_manifest(self):
        for manifest in (False, True):
            with self.subTest(manifest=manifest):
                self.run_launcher(custom=False, manifest=manifest)

    def test_crate_cwd_tree_and_manifest(self):
        for manifest in (False, True):
            with self.subTest(manifest=manifest):
                self.run_launcher(custom=True, manifest=manifest)

    def test_missing_crate_marker_fails_before_running_binary(self):
        for manifest in (False, True):
            with self.subTest(manifest=manifest):
                self.run_launcher(custom=True, manifest=manifest, missing=True)

    def test_windows_expansion_preserves_crlf_on_every_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            launcher = Path(temporary) / "launch.bat"
            expand_launcher(launcher, windows=True, custom_marker=True)
            data = launcher.read_bytes()
            self.assertIn(b"\r\n", data)
            self.assertNotIn(b"\n", data.replace(b"\r\n", b""))


if __name__ == "__main__":
    unittest.main()
