"""No Bazel execution: routing, manifest conservation and real-shell argv probes."""

from collections import Counter
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import linux_bazel_package_shards as shards

ROOT = Path(__file__).resolve().parents[2]
HEAD, TREE = "a" * 40, "b" * 40
ENV = dict(
    GITHUB_ACTIONS="true",
    GITHUB_REPOSITORY=shards.FORK,
    RUNNER_ENVIRONMENT="github-hosted",
    RUNNER_OS="Linux",
    HAS_REMOTE_CREDENTIALS="false",
    GITHUB_RUN_ID="123",
    GITHUB_RUN_ATTEMPT="1",
    GITHUB_EVENT_REPOSITORY_NAME="codex-subagents-multi-provider",
)
TARGETS = sorted(
    [
        "//codex-rs/a:binary",
        "//codex-rs/a:manual-test",
        "//codex-rs/a:suite",
        "//codex-rs/b:unit-tests-bin",
        "//codex-rs/c:library",
        "//codex-rs/d:tool",
        "//codex-rs/e:non-test",
        "//codex-rs/f:custom",
    ]
)


def job_block(name):
    source = (ROOT / ".github/workflows/bazel.yml").read_text()
    return re.search(r"^  " + name + r":\n(.*?)(?=^  [\w-]+:|\Z)", source, re.M | re.S)[
        1
    ]


def step_script(job, name):
    block = job_block(job).split("      - name: " + name + "\n", 1)[1]
    run = block.split("        run: |\n", 1)[1].split("\n      - ", 1)[0]
    return "\n".join(line[10:] for line in run.splitlines()) + "\n"


def fixture_manifest(mode="test", identity=(HEAD, TREE)):
    with (
        patch.object(shards, "git_identity", return_value=identity),
        patch.object(shards, "query", side_effect=[TARGETS, TARGETS]),
    ):
        return shards.create(mode, identity[0], ENV)


class MatrixTests(unittest.TestCase):
    def test_only_keyless_hosted_fork_linux_rows_expand_to_four_fresh_jobs(self):
        enabled, tests, release = shards.plan(ENV)
        self.assertTrue(enabled)
        self.assertEqual(
            Counter(x["target"] for x in tests["include"]),
            {
                "aarch64-apple-darwin": 1,
                "x86_64-apple-darwin": 1,
                "x86_64-unknown-linux-gnu": 4,
                "x86_64-unknown-linux-musl": 4,
            },
        )
        self.assertEqual(len(release["include"]), 6)
        for matrix in (tests, release):
            for target in {
                x["target"] for x in matrix["include"] if x["os"] == "ubuntu-24.04"
            }:
                selected = [x for x in matrix["include"] if x["target"] == target]
                self.assertEqual([x["package_shard"] for x in selected], [1, 2, 3, 4])
                self.assertEqual(len({x["shard_suffix"] for x in selected}), 4)
        windows = release["include"][-1]
        self.assertEqual(
            windows["runs_on"],
            {
                "group": "codex-subagents-multi-provider-runners",
                "labels": "codex-subagents-multi-provider-windows-x64",
            },
        )

    def test_keyed_upstream_other_repository_and_self_hosted_keep_original_rows(self):
        for change in (
            {"HAS_REMOTE_CREDENTIALS": "true"},
            {"GITHUB_REPOSITORY": "openai/codex"},
            {"GITHUB_REPOSITORY": "other/repo"},
            {"RUNNER_ENVIRONMENT": "self-hosted"},
            {"RUNNER_OS": "Windows"},
            {"GITHUB_ACTIONS": "false"},
        ):
            enabled, tests, release = shards.plan(dict(ENV, **change))
            self.assertFalse(enabled)
            self.assertEqual(len(tests["include"]), 4)
            self.assertEqual(len(release["include"]), 3)
            for row in tests["include"] + release["include"]:
                self.assertEqual(
                    (row["package_shard_count"], row["shard_suffix"]), (1, "")
                )
        for value in (None, "", "unknown"):
            with self.assertRaises(ValueError):
                shards.plan(dict(ENV, HAS_REMOTE_CREDENTIALS=value))


class ManifestTests(unittest.TestCase):
    def test_dirty_or_untracked_source_cannot_be_bound_to_head_tree(self):
        with patch.object(
            shards.subprocess, "check_output", return_value=b" M MODULE.bazel.lock\n"
        ):
            with self.assertRaisesRegex(ValueError, "clean source"):
                shards.git_identity()
        with patch.object(
            shards.subprocess, "check_output", return_value=b"?? new/BUILD.bazel\n"
        ):
            with self.assertRaisesRegex(ValueError, "clean source"):
                shards.git_identity()

    def test_shared_manifest_proves_package_expansion_and_conserves_root_assignment(
        self,
    ):
        manifest = fixture_manifest()
        roots = shards.package_roots(TARGETS)
        union = []
        with patch.object(shards, "git_identity", return_value=(HEAD, TREE)):
            for index in range(1, 5):
                union += shards.select(manifest, "test", HEAD, index, ENV)
        self.assertEqual(sorted(union), roots)
        self.assertEqual(len(union), len(set(union)))
        self.assertEqual(manifest["original_labels"], TARGETS)
        # Package wildcards retain manual rules, suites, binaries and libraries;
        # they are not emitted as explicit test labels with altered semantics.
        self.assertEqual(set(roots), {"//codex-rs/" + x + ":all" for x in "abcdef"})
        self.assertNotIn("//codex-rs/a:manual-test", union)

    def test_unequal_expansion_shadowing_duplicate_missing_and_invalid_labels_fail_closed(
        self,
    ):
        for raw in (
            b"",
            b"//x:a\n//x:a\n",
            b"//x:all\n",
            b"@external//x:a\n",
            b"//../x:a\n",
            b"//x:target;echo\n",
        ):
            with self.assertRaises(ValueError):
                shards.labels(raw)
        with (
            patch.object(shards, "git_identity", return_value=(HEAD, TREE)),
            patch.object(shards, "query", side_effect=[TARGETS, TARGETS[:-1]]),
        ):
            with self.assertRaisesRegex(ValueError, "expansion"):
                shards.create("test", HEAD, ENV)
        with self.assertRaises(ValueError):
            shards.assign(["//a:all"], {})
        with self.assertRaises(ValueError):
            shards.assign(["//a:all"] * 4, {})

    def test_source_run_mode_scope_assignment_and_digest_mismatches_are_rejected(self):
        manifest = fixture_manifest()
        changes = [
            {"head": "c" * 40},
            {"tree": "c" * 40},
            {"run_id": "other"},
            {"attempt": "2"},
            {"mode": "release"},
            {"scope": "//narrow/..."},
            {"weights_sha256": "bad"},
            {"labels_sha256": "bad"},
            {"assignments": [["//only:all"]] * 4},
            {"expanded_labels": TARGETS[:-1]},
            {"estimated_recorded_output_bytes": [0] * 4},
        ]
        with patch.object(shards, "git_identity", return_value=(HEAD, TREE)):
            for change in changes:
                with self.subTest(change=change), self.assertRaises(ValueError):
                    shards.select(dict(manifest, **change), "test", HEAD, 1, ENV)
            for index in (0, 5):
                with self.assertRaises(ValueError):
                    shards.select(manifest, "test", HEAD, index, ENV)
        with (
            patch.object(
                shards, "git_identity", side_effect=[(HEAD, TREE), ("c" * 40, TREE)]
            ),
            patch.object(shards, "query", side_effect=[TARGETS, TARGETS]),
        ):
            with self.assertRaisesRegex(ValueError, "source changed"):
                shards.create("test", HEAD, ENV)
        with (
            patch.object(shards, "git_identity", return_value=(HEAD, TREE)),
            patch.object(shards, "query", return_value=["//outside:binary"]),
        ):
            with self.assertRaisesRegex(ValueError, "scope"):
                shards.create("release", HEAD, ENV)

    def test_query_is_time_output_bounded_and_cannot_succeed_with_partial_output(self):
        with patch.object(
            shards, "bounded_query", return_value=("ok", b"//a:x\n")
        ) as query:
            self.assertEqual(shards.query("//...", ENV), ["//a:x"])
            self.assertEqual(
                query.call_args.kwargs, {"timeout": 240, "limit": 8 * 1024 * 1024}
            )
            self.assertEqual(
                query.call_args.args[0][-3:], ["--output=label", "--", "//..."]
            )
        for status in ("timeout", "output-limit", "failed", "incomplete"):
            with patch.object(shards, "bounded_query", return_value=(status, None)):
                with self.assertRaises(ValueError):
                    shards.query("//...", ENV)
        for change in (
            {"BUILDBUDDY_API_KEY": "synthetic"},
            {"RUNNER_OS": "Windows"},
            {"GITHUB_REPOSITORY": "openai/codex"},
        ):
            with self.assertRaises(ValueError):
                shards.create("test", HEAD, dict(ENV, **change))

    def test_estimate_weights_do_not_omit_unknown_packages_or_change_tie_determinism(
        self,
    ):
        roots = ["//p" + str(i) + ":all" for i in range(9)]
        a, loads = shards.assign(roots, {roots[0]: 1000})
        self.assertEqual(
            shards.assign(list(reversed(roots)), {roots[0]: 1000}), (a, loads)
        )
        self.assertEqual(sorted(x for group in a for x in group), roots)
        self.assertTrue(all(a))
        self.assertEqual(sum(loads), 1008)


class WorkflowTests(unittest.TestCase):
    def test_contexts_required_manifest_and_existing_gate_arguments_are_preserved(self):
        planner = job_block("plan-package-shards")
        self.assertIn(
            "HAS_REMOTE_CREDENTIALS: ${{ secrets.BUILDBUDDY_API_KEY != '' }}", planner
        )
        self.assertNotIn("secrets.", planner.split("    steps:")[0])
        manifest = job_block("linux-package-manifest")
        self.assertIn("if-no-files-found: error", manifest)
        self.assertNotIn("continue-on-error", manifest)
        for job in ("test", "verify-release-build"):
            block = job_block(job)
            self.assertIn("needs: [plan-package-shards, linux-package-manifest]", block)
            self.assertIn("!cancelled()", block)
            self.assertIn("needs.plan-package-shards.result == 'success'", block)
            self.assertIn("needs.linux-package-manifest.result == 'success'", block)
            self.assertIn(
                "matrix: ${{ fromJSON(needs.plan-package-shards.outputs.", block
            )
            self.assertNotIn("secrets.", block.split("    steps:")[0])
            self.assertIn(
                "cache-scope: bazel-${{ github.job }}${{ matrix.shard_suffix }}", block
            )
            self.assertIn(
                "name: hosted-storage-${{ github.job }}-${{ matrix.target }}${{ matrix.shard_suffix }}",
                block,
            )
            self.assertIn('--expected-head "$GITHUB_SHA"', block)
            self.assertIn('-- \\\n            "${bazel_targets[@]}"', block)
        test = step_script("test", "bazel test //...")
        self.assertIn("--test_tag_filters=-argument-comment-lint", test)
        self.assertIn("-//third_party/v8:all", test)
        self.assertIn("-//codex-rs/v8-poc:v8-poc-unit-tests", test)
        self.assertIn(
            'bazel_targets=("${package_roots[@]}" "${bazel_targets[@]:1}")', test
        )
        release = step_script(
            "verify-release-build", "bazel build verify-release-build targets"
        )
        self.assertIn("extra_rustc_flag=-Cdebug-assertions=no", release)
        self.assertIn("extra_exec_rustc_flag=-Cdebug-assertions=no", release)
        self.assertIn("--compilation_mode=fastbuild", release)
        self.assertIn(
            'bazel_targets=("${package_roots[@]}" "${bazel_targets[@]:1}")', release
        )
        self.assertIn("Unexpected additional positive target pattern", release)
        self.assertIn("//codex-rs/bwrap:bwrap", job_block("verify-release-build"))


class ShellArgumentTests(unittest.TestCase):
    def test_unsharded_keyed_upstream_macos_and_windows_keep_original_target_arguments(
        self,
    ):
        bash = (
            str(
                Path(os.environ.get("ProgramFiles", "C:/Program Files"))
                / "Git/bin/bash.exe"
            )
            if os.name == "nt"
            else shutil.which("bash")
        )
        self.assertTrue(bash and Path(bash).exists())
        for mode, job, step in (
            ("test", "test", "bazel test //..."),
            (
                "release",
                "verify-release-build",
                "bazel build verify-release-build targets",
            ),
        ):
            for change in (
                {"BUILDBUDDY_API_KEY": "synthetic"},
                {"GITHUB_REPOSITORY": "openai/codex"},
                {"RUNNER_OS": "macOS"},
                {"RUNNER_OS": "Windows"},
            ):
                with (
                    self.subTest(mode=mode, change=change),
                    tempfile.TemporaryDirectory() as directory,
                ):
                    temp = Path(directory)
                    record = temp / "argv.json"
                    stub = temp / "wrapper.py"
                    stub.write_text(
                        "import json,sys\nfrom pathlib import Path\nPath("
                        + repr(str(record))
                        + ").write_text(json.dumps(sys.argv[1:]))\n",
                        newline="\n",
                    )
                    script = step_script(job, step).replace(
                        "./.github/scripts/run-bazel-ci.sh",
                        'python3 "' + str(stub).replace("\\", "/") + '"',
                    )
                    path = temp / "step.sh"
                    path.write_text("set -euo pipefail\n" + script, newline="\n")
                    env = {
                        k: v
                        for k, v in os.environ.items()
                        if k in ("PATH", "SYSTEMROOT", "SystemRoot", "TEMP", "TMP")
                    }
                    env.update(
                        ENV,
                        RUNNER_TEMP=str(temp).replace("\\", "/"),
                        GITHUB_SHA=HEAD,
                        LINUX_PACKAGE_SHARD="1",
                        LINUX_PACKAGE_SHARD_COUNT="1",
                        MSYS2_ARG_CONV_EXCL="*",
                    )
                    env.update(change)
                    result = subprocess.run(
                        [bash, str(path)],
                        cwd=ROOT,
                        env=env,
                        text=True,
                        capture_output=True,
                        timeout=30,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    argv = json.loads(record.read_text())
                    second = [i for i, x in enumerate(argv) if x == "--"][1]
                    self.assertEqual(
                        argv[second + 1 :],
                        [
                            "//...",
                            "-//third_party/v8:all",
                            "-//codex-rs/v8-poc:v8-poc-unit-tests",
                        ]
                        if mode == "test"
                        else [
                            "//codex-rs/...",
                            "-//codex-rs/core/tests/remote_env_windows:smoke-test",
                            "-//codex-rs/v8-poc:all",
                        ],
                    )
                    self.assertFalse((temp / "linux-package-manifests").exists())
                    if mode == "release" and change.get("RUNNER_OS") == "Windows":
                        self.assertIn("--windows-cross-compile", argv)

    def test_real_shell_retains_package_wildcards_negatives_and_release_flags(self):
        bash = (
            str(
                Path(os.environ.get("ProgramFiles", "C:/Program Files"))
                / "Git/bin/bash.exe"
            )
            if os.name == "nt"
            else shutil.which("bash")
        )
        self.assertTrue(
            bash and Path(bash).exists(), "Git Bash/bash required for this probe"
        )
        identity = shards.git_identity()
        for job, name, mode in (
            ("test", "bazel test //...", "test"),
            (
                "verify-release-build",
                "bazel build verify-release-build targets",
                "release",
            ),
        ):
            manifest = fixture_manifest(mode, identity)
            with tempfile.TemporaryDirectory() as directory:
                temp = Path(directory)
                (temp / "linux-package-manifests").mkdir()
                (temp / "linux-package-manifests" / (mode + ".json")).write_text(
                    json.dumps(manifest)
                )
                record = temp / "record.json"
                stub = temp / "wrapper.py"
                stub.write_text(
                    "import json,sys\nfrom pathlib import Path\nPath("
                    + repr(str(record))
                    + ").write_text(json.dumps(sys.argv[1:]))\n",
                    newline="\n",
                )
                script = step_script(job, name).replace(
                    "./.github/scripts/run-bazel-ci.sh",
                    'python3 "' + str(stub).replace("\\", "/") + '"',
                )
                path = temp / "step.sh"
                path.write_text("set -euo pipefail\n" + script, newline="\n")
                env = {
                    k: v
                    for k, v in os.environ.items()
                    if k in ("PATH", "SYSTEMROOT", "SystemRoot", "TEMP", "TMP")
                }
                env.update(
                    ENV,
                    RUNNER_TEMP=str(temp).replace("\\", "/"),
                    GITHUB_SHA=identity[0],
                    LINUX_PACKAGE_SHARD="1",
                    LINUX_PACKAGE_SHARD_COUNT="4",
                    MSYS2_ARG_CONV_EXCL="*",
                )
                result = subprocess.run(
                    [bash, str(path)],
                    cwd=ROOT,
                    env=env,
                    text=True,
                    capture_output=True,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                argv = json.loads(record.read_text())
                # The wrapper's second separator starts targets; no target can
                # become a flag or be expanded by the shell.
                separators = [i for i, value in enumerate(argv) if value == "--"]
                self.assertEqual(len(separators), 2)
                payload = argv[separators[1] + 1 :]
                self.assertEqual(
                    payload[: len(manifest["assignments"][0])],
                    manifest["assignments"][0],
                )
                self.assertEqual(
                    payload[len(manifest["assignments"][0]) :],
                    ["-//third_party/v8:all", "-//codex-rs/v8-poc:v8-poc-unit-tests"]
                    if mode == "test"
                    else [
                        "-//codex-rs/core/tests/remote_env_windows:smoke-test",
                        "-//codex-rs/v8-poc:all",
                    ],
                )
                self.assertNotIn("//codex-rs/a:manual-test", payload)
                if mode == "release":
                    self.assertIn(
                        "--@rules_rust//rust/settings:extra_rustc_flag=-Cdebug-assertions=no",
                        argv,
                    )
                    self.assertIn(
                        "--@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebug-assertions=no",
                        argv,
                    )


if __name__ == "__main__":
    unittest.main()
