"""Bounded configured metadata only; never executes an action or a selected tool.

Raw wrapper output, including its merged stderr, stays in private/. Only validated
JSON goes in upload/. Environment values are removed, including PATH/MSVC paths;
artifact paths, action arguments, param-file arguments and depsets remain intact.
Unknown schema/environment names fail closed. Bazel schema: 9.2.0
src/main/protobuf/analysis_v2.proto. This is not compiler/lint/runtime proof.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from voice_metadata_owned_job import capture_owned

LIMIT = 32 * 1024 * 1024
TOTAL = 128 * 1024 * 1024
MANIFEST_RESERVE = 64 * 1024
PREFIX = "//codex-rs/voice-host:"
NAMES = (
    "codex-voice-host",
    "codex-voice-host__rust",
    "codex-voice-host__windows-runtime",
    "codex-voice-host-bin-unit-tests-bin",
    "codex-voice-host-bin-unit-tests-bin__windows-runtime",
    "codex-voice-host-bin-unit-tests",
    "voice-host-installed_client-test",
)
LABELS = tuple(PREFIX + x for x in NAMES)
QUERY = (
    "deps(set("
    + " ".join(
        PREFIX + x
        for x in (
            "codex-voice-host-bin-unit-tests",
            "voice-host-installed_client-test",
            "voice-host-windows-runtime-diagnostic-test",
        )
    )
    + "))"
)
SENTINEL = "VOICE_PROVIDER_JSON:"
ENV_NAMES = set(
    """PATH SystemRoot SYSTEMROOT PROCESSOR_ARCHITECTURE INCLUDE LIB
LIBPATH UCRTVersion UniversalCRTSdkDir VCINSTALLDIR VCToolsInstallDir WindowsLibPath
WindowsSdkBinPath WindowsSdkDir WindowsSDKLibVersion WindowsSDKVersion
RUST_TEST_THREADS CARGO_MANIFEST_DIR CARGO_PKG_NAME CARGO_PKG_VERSION
CARGO_PKG_VERSION_MAJOR CARGO_PKG_VERSION_MINOR CARGO_PKG_VERSION_PATCH
CARGO_PKG_VERSION_PRE CARGO_PKG_AUTHORS CARGO_PKG_DESCRIPTION CARGO_PKG_HOMEPAGE
CARGO_PKG_REPOSITORY CARGO_PKG_LICENSE CARGO_PKG_LICENSE_FILE CARGO_CRATE_NAME
CARGO_BIN_NAME CARGO_PRIMARY_PACKAGE CARGO_CFG_TARGET_ARCH CARGO_CFG_TARGET_OS
CARGO_CFG_TARGET_ENV OUT_DIR NUM_JOBS OPT_LEVEL DEBUG HOST TARGET RUST_BACKTRACE
RUST_LOG RUSTC_BOOTSTRAP CODEX_GIT_COMMIT_HASH CODEX_BUILD_COMMIT
BUILD_SCM_REVISION STABLE_BUILD_SCM_REVISION SOURCE_DATE_EPOCH""".split()
)
SENSITIVE = re.compile(
    r"(?i)(?:authorization|remote[_-]header|api[_-]?key|access[_-]?token|password|secret)"
)


class EvidenceError(Exception):
    """Intentionally contains only static diagnostics, never raw query content."""


def require(ok, message):
    if not ok:
        raise EvidenceError(message)


def encode(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "Duplicate JSON object key")
        result[key] = value
    return result


def parse_json(text):
    """Decode one JSON object, allowing only recognized wrapper/Bazel log lines."""

    def logs(lines):
        for line in lines.splitlines():
            if not line.strip():
                continue
            require(
                bool(
                    re.match(
                        r"^(?:INFO:|DEBUG:|WARNING:|Loading:|Analyzing:|Computing main repo mapping:|"
                        r"BuildBuddy API key is not available;|Windows fallback:)",
                        line,
                    )
                ),
                "Unexpected wrapper output outside JSON",
            )

    # JSON begins at a line boundary. A log containing a brace cannot consume it.
    starts = list(re.finditer(r"(?m)^\s*\{", text))
    require(bool(starts), "Missing JSON object")
    start = starts[0].start()
    logs(text[:start])
    try:
        result, end = json.JSONDecoder(object_pairs_hook=unique_object).raw_decode(
            text[start:].lstrip()
        )
    except (ValueError, RecursionError):
        raise EvidenceError("Malformed or truncated JSON") from None
    offset = len(text[start:]) - len(text[start:].lstrip())
    logs(text[start + offset + end :])
    require(isinstance(result, dict), "Expected JSON object")
    return result


def parse_provider(text, label):
    records = [
        line[len(SENTINEL) :] for line in text.splitlines() if line.startswith(SENTINEL)
    ]
    require(len(records) == 1, "Expected exactly one provider sentinel record")
    # Validate all remaining merged output as logs, rejecting formatter errors.
    parse_json(
        "\n".join(line for line in text.splitlines() if not line.startswith(SENTINEL))
        + "\n{}"
    )
    try:
        record = json.loads(records[0], object_pairs_hook=unique_object)
    except ValueError:
        raise EvidenceError("Malformed provider record") from None
    require(record.get("label") == label, "Provider label mismatch")
    require(isinstance(record.get("provider_keys"), list), "Missing provider keys")
    require("default" in record, "Missing DefaultInfo")
    require(
        set(record["default"])
        == {"files", "executable", "default_runfiles", "data_runfiles"},
        "Unknown DefaultInfo shape",
    )
    return record


def env_allowed(name):
    return name in ENV_NAMES or name.startswith("CARGO_BIN_EXE_")


def sanitize_environment(env):
    require(isinstance(env, dict), "Unknown environment shape")
    unknown = sorted(k for k in env if not env_allowed(k))
    if unknown:
        # Names are review evidence; never interpolate values or arbitrary text.
        safe_names = [
            k if re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]{0,127}", k) else "<invalid-name>"
            for k in unknown
        ]
        raise EvidenceError("Unexpected environment names: " + ", ".join(safe_names))
    return {k: "<redacted>" for k in env}


def safe_arguments(args):
    require(
        isinstance(args, list) and all(isinstance(x, str) for x in args),
        "Unknown argument shape",
    )
    for arg in args:
        require(not SENSITIVE.search(arg), "Sensitive action argument rejected")
        # Preserve paths and normal arguments. Environment assignments can carry
        # arbitrary values even when the variable name looks harmless.
        require(
            not re.match(r"^(?:--(?:host_)?action_env=|--env=)", arg),
            "Embedded environment option requires review",
        )
        assignment = re.match(r"^([A-Za-z_][A-Za-z_0-9]*)=", arg)
        require(not assignment, "Embedded environment assignment requires review")
    return args


def sanitize_provider(raw):
    result = copy.deepcopy(raw)
    known = {
        "label",
        "provider_keys",
        "default",
        "run_environment",
        "output_groups",
        "voice_package",
    }
    for key, value in result.items():
        if key in known:
            continue
        require(
            ("CrateInfo" in key or "crate_info" in key)
            and set(value) == {"fields", "selected"},
            "Unknown provider section",
        )
        if "rustc_env" in value["selected"]:
            # Full equality is computed before this irreversible removal.
            value["selected"]["rustc_env"] = {
                k: "<redacted>" for k in value["selected"]["rustc_env"]
            }
    if "run_environment" in result:
        e = result["run_environment"]
        require(
            set(e) == {"environment", "inherited_environment_names"},
            "Unknown RunEnvironmentInfo shape",
        )
        e["environment"] = sanitize_environment(e["environment"])
        require(
            all(env_allowed(k) for k in e["inherited_environment_names"]),
            "Unexpected inherited environment",
        )
    return result


# Exact JSON names from pinned analysis_v2.proto. Reject additions until reviewed.
SCHEMA = {
    "artifacts": "id pathFragmentId isTreeArtifact",
    "actions": """targetId aspectDescriptorIds actionKey mnemonic configurationId
        arguments environmentVariables inputDepSetIds outputIds discoversInputs
        executionInfo paramFiles primaryOutputId executionPlatform templateContent
        substitutions fileContents unresolvedSymlinkTarget isExecutable""",
    "targets": "id label ruleClassId",
    "depSetOfFiles": "id transitiveDepSetIds directArtifactIds",
    "configuration": "id mnemonic platformName checksum isTool",
    "aspectDescriptors": "id name parameters",
    "ruleClasses": "id name",
    "pathFragments": "id label parentId",
}


def graph_metadata(raw):
    require(set(raw) <= set(SCHEMA), "Unknown action graph section")
    graph = copy.deepcopy(raw)
    tables = {}
    for section, fields in SCHEMA.items():
        rows = graph.get(section, [])
        require(isinstance(rows, list), "Unknown graph table shape")
        for row in rows:
            require(
                isinstance(row, dict) and set(row) <= set(fields.split()),
                "Unknown action graph field",
            )
        if section != "actions":
            table = {}
            for row in rows:
                key = str(row.get("id", ""))
                require(
                    key not in ("", "0") and key not in table,
                    "Missing or duplicate graph ID",
                )
                table[key] = row
            tables[section] = table

    def get(section, key):
        require(str(key) in tables[section], "Missing graph reference")
        return tables[section][str(key)]

    paths, deps = {}, {}

    def path(key, active=None):
        key = str(key)
        if key in paths:
            return paths[key]
        active = set() if active is None else active
        require(key not in active, "Path fragment cycle")
        row = get("pathFragments", key)
        parent = row.get("parentId", 0)
        value = (
            path(parent, active | {key}) + "/" if str(parent) != "0" else ""
        ) + row["label"]
        paths[key] = value
        return value

    artifacts = {
        key: path(row.get("pathFragmentId", 0))
        for key, row in tables["artifacts"].items()
    }

    def artifact(key):
        require(str(key) in artifacts, "Missing artifact ID")
        return artifacts[str(key)]

    def dep(key, active=None):
        key = str(key)
        if key in deps:
            return deps[key]
        active = set() if active is None else active
        require(key not in active, "Depset cycle")
        row = get("depSetOfFiles", key)
        value = {artifact(i) for i in row.get("directArtifactIds", [])}
        for child in row.get("transitiveDepSetIds", []):
            value.update(dep(child, active | {key}))
        deps[key] = value
        return value

    for key in tables["pathFragments"]:
        path(key)
    for key in tables["depSetOfFiles"]:
        dep(key)
    for target in tables["targets"].values():
        get("ruleClasses", target.get("ruleClassId", 0))
    resolved = []
    for action in graph.get("actions", []):
        owner = get("targets", action.get("targetId", 0))["label"]
        if str(action.get("configurationId", 0)) != "0":
            get("configuration", action["configurationId"])
        for key in action.get("aspectDescriptorIds", []):
            get("aspectDescriptors", key)
        inputs = set()
        for key in action.get("inputDepSetIds", []):
            inputs.update(dep(key))
        outputs = [artifact(key) for key in action.get("outputIds", [])]
        if str(action.get("primaryOutputId", 0)) != "0":
            require(
                artifact(action["primaryOutputId"]) in outputs,
                "Primary output absent from outputs",
            )
        env = action.get("environmentVariables", [])
        require(
            all(set(p) <= {"key", "value"} and "key" in p for p in env),
            "Unknown action environment shape",
        )
        require(len({p["key"] for p in env}) == len(env), "Duplicate environment name")
        sanitize_environment({p["key"]: p.get("value", "") for p in env})
        for p in env:
            p["value"] = "<redacted>"
        safe_arguments(action.get("arguments", []))
        for param in action.get("paramFiles", []):
            require(set(param) <= {"execPath", "arguments"}, "Unknown param file shape")
            safe_arguments(param.get("arguments", []))
        # Execution properties may contain credentials; retain names only, and
        # fail unexpected property shapes. No environment/headers are uploaded.
        for prop in action.get("executionInfo", []):
            require(set(prop) <= {"key", "value"}, "Unknown execution property shape")
            prop["value"] = "<redacted>"
        for substitution in action.get("substitutions", []):
            require(set(substitution) <= {"key", "value"}, "Unknown substitution shape")
            # Arbitrary generated text can reproduce environment values with no
            # recognizable variable name or credential word. Retain shape only.
            for key in substitution:
                substitution[key] = "<redacted>"
        for field in ("templateContent", "fileContents", "unresolvedSymlinkTarget"):
            if field in action:
                action[field] = "<redacted>"
        resolved.append(
            {
                "owner": owner,
                "mnemonic": action.get("mnemonic"),
                "inputs": sorted(inputs),
                "outputs": outputs,
            }
        )
    for descriptor in graph.get("aspectDescriptors", []):
        for parameter in descriptor.get("parameters", []):
            require(
                set(parameter) <= {"key", "value"}, "Unknown aspect parameter shape"
            )
            require(
                not SENSITIVE.search(parameter.get("key", ""))
                and not SENSITIVE.search(parameter.get("value", "")),
                "Sensitive aspect parameter rejected",
            )
    return graph, resolved


def package_proof(providers, actions):
    by_label = {p["label"]: p for p in providers}
    copies = [a for a in actions if a["mnemonic"] == "VoiceExecutablePackage"]
    require(len(copies) == 2, "Expected exactly two package actions")
    proofs = []
    for raw_name, package_name in ((NAMES[1], NAMES[2]), (NAMES[3], NAMES[4])):
        raw, package = (by_label[PREFIX + n] for n in (raw_name, package_name))
        p = package.get("voice_package")
        require(
            p is not None
            and set(p) == {"raw_executable", "executable", "dlls", "receipt"},
            "Missing or unknown package provider",
        )
        d = package["default"]
        require(
            d["files"] == [p["executable"]] and d["executable"] == p["executable"],
            "Package executable identity mismatch",
        )
        require(
            raw["default"]["executable"] == p["raw_executable"]
            and p["raw_executable"] != p["executable"],
            "Raw executable mismatch",
        )
        require(
            len(p["dlls"]) == 25 and len(set(p["dlls"])) == 25,
            "Expected 25 distinct packaged DLLs",
        )
        for kind in ("default_runfiles", "data_runfiles"):
            require(
                set(p["dlls"] + [p["receipt"]]) <= set(d[kind]),
                "Missing packaged runfiles",
            )
            require(set(raw["default"][kind]) <= set(d[kind]), "Lost upstream runfiles")
        owned = [a for a in copies if a["owner"] == package["label"]]
        require(len(owned) == 1, "Package action owner mismatch")
        require(
            len(owned[0]["outputs"]) == 27
            and set(owned[0]["outputs"])
            == set([p["executable"], p["receipt"]] + p["dlls"]),
            "Package outputs mismatch",
        )
        require(
            p["raw_executable"] in owned[0]["inputs"], "Raw executable input missing"
        )
        require(
            raw.get("output_groups") == package.get("output_groups"),
            "Output groups differ",
        )
        require(
            raw.get("run_environment") == package.get("run_environment"),
            "Run environments differ",
        )
        proofs.append(
            {
                "package": package["label"],
                "raw": raw["label"],
                "output_count": 27,
                "output_groups_equal": True,
                "run_environment_equal_including_private_values": True,
            }
        )
    public = by_label[LABELS[0]]["default"]["executable"]
    require(
        public == by_label[LABELS[2]]["voice_package"]["executable"],
        "Public helper is not packaged executable",
    )
    return proofs


def capture(command, destination, budget, timeout=45):
    cap = max(0, min(LIMIT, TOTAL - MANIFEST_RESERVE - budget[0]))
    result = capture_owned(command, destination, cap, timeout)
    budget[0] += result["retained_bytes"]
    return result


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--expected-head", required=True)
    parser.add_argument("--expected-tree", required=True)
    parser.add_argument("--git-bash", type=Path, required=True)
    args = parser.parse_args()
    require(
        os.name == "nt" and os.environ.get("PROCESSOR_ARCHITECTURE") == "AMD64",
        "Configured capture requires native Windows AMD64",
    )
    require(
        args.git_bash.is_absolute()
        and args.git_bash.is_file()
        and args.git_bash.name.lower() in {"bash", "bash.exe"},
        "Expected qualified absolute current Git Bash",
    )
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    # mkdtemp guarantees this invocation cannot attach to a reusable server or
    # the ordinary runtime output root. Batch further forbids daemon reuse.
    query_root = Path(tempfile.mkdtemp(prefix="voice-metadata-bazel-", dir=runner_temp))
    os.environ["BAZEL_OUTPUT_USER_ROOT"] = str(query_root)
    require(
        git("rev-parse", "HEAD") == args.expected_head
        and git("rev-parse", "HEAD^{tree}") == args.expected_tree,
        "Checkout identity mismatch",
    )
    require(
        not git("status", "--porcelain", "--untracked-files=normal"),
        "Checkout must be clean",
    )
    args.root.mkdir()  # Fail if stale evidence directory already exists.
    private, upload = args.root / "private", args.root / "upload"
    private.mkdir()
    upload.mkdir()
    hashes = {
        str(p): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in (
            Path("MODULE.bazel"),
            Path("MODULE.bazel.lock"),
            Path(".bazelrc"),
            Path("third_party/voice/windows_executable.bzl"),
        )
    }
    env = os.environ.pop("BUILDBUDDY_API_KEY", None)
    os.environ.pop("CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR", None)
    manifest = {
        "head": args.expected_head,
        "tree": args.expected_tree,
        "scope": "configured metadata only; no actions executed",
        "phases": [],
        "source_sha256": hashes,
        "status": "failed",
        "provider_semantics": "unknown until actual hosted provider keys and fields are reviewed",
        "unexecuted_gates": [
            "compiler identity",
            "Rustc input dependency proof",
            "strict clippy",
            "argument-comment-lint",
            "symbol materialization",
            "native ARM",
            "release staging",
        ],
    }
    budget, providers = [0], []
    try:
        for i, label in enumerate(LABELS):
            output = private / f"provider-{i}.raw"
            cmd = [
                str(args.git_bash),
                "./.github/scripts/run-bazel-ci.sh",
                "--windows-cross-compile",
                "--",
                "--batch",
                "cquery",
                "--output=starlark",
                "--starlark:file=.github/scripts/voice_providers.cquery",
                "--lockfile_mode=error",
                "--color=no",
                "--curses=no",
                "--",
                label,
            ]
            result = capture(cmd, output, budget)
            manifest["phases"].append({"phase": f"provider-{i}", **result})
            require(
                not result["oversize"]
                and not result["timeout"]
                and result["exit_code"] == 0,
                "Provider capture failed; raw output retained privately",
            )
            providers.append(parse_provider(output.read_text(encoding="utf-8"), label))
        output = private / "actions.raw"
        cmd = [
            str(args.git_bash),
            "./.github/scripts/run-bazel-ci.sh",
            "--windows-cross-compile",
            "--",
            "--batch",
            "aquery",
            "--output=jsonproto",
            "--include_commandline",
            "--include_artifacts",
            "--include_param_files",
            "--include_file_write_contents",
            "--lockfile_mode=error",
            "--color=no",
            "--curses=no",
            "--",
            QUERY,
        ]
        result = capture(cmd, output, budget, timeout=90)
        manifest["phases"].append({"phase": "actions", **result})
        require(
            not result["oversize"]
            and not result["timeout"]
            and result["exit_code"] == 0,
            "Action capture failed; raw output retained privately",
        )
        graph, actions = graph_metadata(parse_json(output.read_text(encoding="utf-8")))
        proof = package_proof(providers, actions)
        artifacts = {
            "providers.json": [sanitize_provider(p) for p in providers],
            "actions.json": graph,
            "resolved-actions.json": actions,
            "package-proof.json": proof,
        }
        # Serialize every artifact before writing any; a failed sanitizer uploads
        # no partially sanitized graph. Limits apply to complete upload files too.
        payloads = {name: encode(value) for name, value in artifacts.items()}
        require(
            all(len(v) <= LIMIT for v in payloads.values())
            and budget[0] + sum(map(len, payloads.values()))
            <= TOTAL - MANIFEST_RESERVE,
            "Sanitized evidence exceeds cap",
        )
        manifest["artifacts"] = {
            name: {"bytes": len(value), "sha256": hashlib.sha256(value).hexdigest()}
            for name, value in payloads.items()
        }
        for name, value in payloads.items():
            (upload / name).write_bytes(value)
        manifest["status"] = "configured-graph-and-package-structure-validated"
    except EvidenceError as error:
        manifest["failure"] = str(error)
        raise
    finally:
        unchanged = (
            git("rev-parse", "HEAD") == args.expected_head
            and not git("status", "--porcelain", "--untracked-files=normal")
            and all(
                hashlib.sha256(Path(p).read_bytes()).hexdigest() == digest
                for p, digest in hashes.items()
            )
        )
        manifest["source_unchanged"] = unchanged
        if not unchanged:
            manifest["status"] = "failed-source-changed"
        (upload / "manifest.json").write_bytes(encode(manifest))
        if env is not None:
            os.environ["BUILDBUDDY_API_KEY"] = env
        require(unchanged, "Pinned source or lock changed")
    print("Configured metadata validated; sanitized evidence ready.")


if __name__ == "__main__":
    try:
        main()
    except (EvidenceError, OSError, ValueError, KeyError, RecursionError, TypeError):
        # No exception interpolation: JSON parser/subprocess errors can contain
        # secrets from raw input. Raw diagnostics are private, never console.
        print(
            "Configured metadata failed; inspect private evidence locally.",
            file=sys.stderr,
        )
        sys.exit(1)
