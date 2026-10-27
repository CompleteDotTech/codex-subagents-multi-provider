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
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

from voice_metadata_owned_job import capture_owned

LIMIT = 32 * 1024 * 1024
TOTAL = 128 * 1024 * 1024
MANIFEST_RESERVE = 64 * 1024
# The Actions step remains 600s. Reserve 30s outside this invocation envelope
# for shell/startup/scheduling, then 60s for final parsing/sanitization/source
# checks. These allowances are policy, not hard OS/Git latency guarantees.
INVOCATION_SECONDS = 570
POSTPROCESSING_RESERVE_SECONDS = 60
OWNED_CLEANUP_RESERVE_SECONDS = 15  # Three existing sequential waits of up to 5s.
FIRST_PROVIDER_SECONDS = 120
LATER_PROVIDER_SECONDS = 45
ACTIONS_SECONDS = 90
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


class RejectedWrapperLine(EvidenceError):
    """One fixed shape observation; never retains the rejected private line."""

    def __init__(self, line):
        super().__init__("Unexpected wrapper output outside JSON")
        # Inspect only 64 code points, independent of the private line length.
        # UiStateTracker.java:90,1179 (Bazel 9.2.0) emits the fixed fetch detail
        # prefix. Spoofed text can match; this neither authenticates a source nor
        # authorizes the line. Absence excludes only these exact prefix shapes.
        prefix = line[:64]
        four_spaces = prefix.startswith("    ")
        self.shape = {
            "schema": "rejected-wrapper-line-shape-v1",
            "starts_with_four_ascii_spaces": four_spaces,
            "other_ascii_leading_whitespace": bool(prefix)
            and prefix[0] in " \t\r\n\v\f"
            and not four_spaces,
            "esc_control_in_prefix": "\x1b" in prefix,
            "fixed_bazel_continuation": "fetch-detail"
            if prefix.startswith("    Fetching ")
            else "none",
        }


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
            # Bazel 9.2.0 archive_utils.cc:248 emits this complete USER line.
            if line == "Extracting Bazel installation...":
                continue
            if not re.match(
                r"^(?:INFO:|DEBUG:|WARNING:|Loading:|Analyzing:|Computing main repo mapping:|"
                r"BuildBuddy API key is not available;|Windows fallback:)",
                line,
            ):
                raise RejectedWrapperLine(line)

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


# Independent from the metadata sanitizer: only diagnostic child-process inputs.
# Python normalizes os.environ keys to uppercase on Windows. Git Bash's
# indirect SDK lookups are case sensitive: emit the exact source spellings.
SDK_ENV_NAMES = """INCLUDE LIB LIBPATH UCRTVersion UniversalCRTSdkDir
VCINSTALLDIR VCToolsInstallDir WindowsLibPath WindowsSdkBinPath WindowsSdkDir
WindowsSDKLibVersion WindowsSDKVersion""".split()
SDK_ENV = frozenset(name.upper() for name in SDK_ENV_NAMES)
CHILD_ENV_SPELLINGS = {name.upper(): name for name in SDK_ENV_NAMES}
# Canonical lowercase proxy aliases preserve Windows' case-insensitive values
# while making HTTP proxy policy visible to case-sensitive Git Bash/curl tools.
CHILD_ENV_SPELLINGS.update(
    {
        name.upper(): name
        for name in (
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        )
    }
)
# PATH is the thirteenth exported SDK input in setup-bazel-ci/action.yml.
REQUIRED_CHILD_ENV = SDK_ENV | frozenset(
    """PATH SYSTEMROOT PROCESSOR_ARCHITECTURE
HOME USERPROFILE TEMP TMP RUNNER_OS RUNNER_TEMP GITHUB_ACTIONS GITHUB_REPOSITORY
GITHUB_EVENT_NAME GITHUB_SHA GITHUB_REF GITHUB_RUN_ID GITHUB_JOB GITHUB_WORKSPACE
VOICE_DIAGNOSTIC_HEAD VOICE_DIAGNOSTIC_TREE CODEX_BAZEL_WINDOWS_PATH
VOICE_WINDOWS_SYSTEM_ROOT VOICE_WINDOWS_HOST_ARCH VOICE_WINDOWS_BAZEL_REPOSITORY""".split()
)
OPTIONAL_CHILD_ENV = frozenset(
    """SYSTEMDRIVE WINDIR COMSPEC PATHEXT
PROCESSOR_ARCHITEW6432 OS HOMEDRIVE HOMEPATH APPDATA LOCALAPPDATA TMPDIR TEMPDIR
PROGRAMFILES PROGRAMFILES(X86) PROGRAMW6432 SHELL MSYSTEM MSYS MSYS2_PATH_TYPE
MSYS2_ARG_CONV_EXCL MSYS2_ENV_CONV_EXCL MSYS_NO_PATHCONV CHERE_INVOKING BASH_ENV ENV LANG LC_ALL
LC_CTYPE TZ PYTHONHOME PYTHONPATH PYTHONUTF8 PYTHONIOENCODING PYTHONNOUSERSITE
CODEX_BAZEL_BIN CODEX_REPO_ROOT CI_BUILD_ROOT CARGO_TARGET_DIR
CARGO_NET_GIT_FETCH_WITH_CLI BAZEL_OUTPUT_BASE
BAZEL_REPOSITORY_CACHE BAZEL_REPO_CONTENTS_CACHE BAZEL_SH BAZEL_VC
BAZEL_VC_FULL_VERSION BAZEL_WINSDK_FULL_VERSION BAZELISK_HOME USE_BAZEL_VERSION
JAVA_HOME JAVA_TOOL_OPTIONS JDK_JAVA_OPTIONS _JAVA_OPTIONS VOICE_CYGWIN_ROOT
VOICE_PKG_CONFIG GITHUB_EVENT_PATH HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY
SSL_CERT_FILE SSL_CERT_DIR REQUESTS_CA_BUNDLE CURL_CA_BUNDLE GIT_SSL_CAINFO
GIT_SSL_CAPATH GIT_SSL_NO_VERIFY CURL_HOME NODE_EXTRA_CA_CERTS""".split()
)


def utf16_units(value):
    try:
        return len(value.encode("utf-16-le")) // 2
    except (UnicodeError, AttributeError):
        raise EvidenceError("Invalid diagnostic environment encoding") from None


def environment_accounting(environment):
    return {
        "entry_count": len(environment),
        "block_utf16_units": (1 if environment else 2)
        + sum(utf16_units(k + "=" + v) + 1 for k, v in environment.items()),
        # Projection only: Git Bash may transform paths, and Bazel adds rc/JVM
        # arguments downstream. This is never a final-command acceptance gate.
        "unescaped_client_env_utf16_units": sum(
            utf16_units("--client_env=" + k + "=" + v) + 1
            for k, v in environment.items()
        ),
    }


def diagnostic_environment(parent, query_root, expected_head, expected_tree):
    child, indexed = {}, {}
    for name, value in parent.items():
        upper = name.upper()
        if upper in REQUIRED_CHILD_ENV | OPTIONAL_CHILD_ENV:
            require(upper not in indexed, "Conflicting diagnostic environment aliases")
            indexed[upper] = value
            child[CHILD_ENV_SPELLINGS.get(upper, name)] = value
    require(
        all(indexed.get(name) for name in REQUIRED_CHILD_ENV),
        "Missing required diagnostic environment",
    )
    require(
        indexed["RUNNER_OS"] == "Windows"
        and indexed["GITHUB_ACTIONS"] == "true"
        and indexed["GITHUB_REPOSITORY"]
        == "CompleteDotTech/codex-subagents-multi-provider"
        and indexed["GITHUB_EVENT_NAME"] == "workflow_dispatch"
        and indexed["GITHUB_JOB"] == "voice-runtime-diagnostic"
        and indexed["PROCESSOR_ARCHITECTURE"] == "AMD64"
        and indexed["VOICE_WINDOWS_HOST_ARCH"]
        == indexed.get("PROCESSOR_ARCHITEW6432", indexed["PROCESSOR_ARCHITECTURE"])
        and indexed["VOICE_WINDOWS_SYSTEM_ROOT"] == indexed["SYSTEMROOT"],
        "Diagnostic environment identity mismatch",
    )
    require(
        indexed["VOICE_DIAGNOSTIC_HEAD"] == expected_head
        and indexed["VOICE_DIAGNOSTIC_TREE"] == expected_tree
        and indexed["GITHUB_SHA"] == expected_head,
        "Diagnostic checkout environment identity mismatch",
    )
    # Only this child receives the unique output root. Credentials and execution
    # log settings are absent from the explicit allowlist, never popped globally.
    child["BAZEL_OUTPUT_USER_ROOT"] = str(query_root)
    child["VOICE_CONFIGURED_METADATA_MODE"] = "bounded-v1"
    from voice_metadata_owned_job import environment_block

    try:
        block = environment_block(child)
    except ValueError:
        raise EvidenceError("Invalid diagnostic child environment") from None
    metrics = {
        "parent": environment_accounting(parent),
        "child": environment_accounting(child),
        "child_block_utf16_units": len(block) // 2,
        "included_values_unchanged": all(
            indexed[k.upper()] == v
            for k, v in child.items()
            if k not in {"BAZEL_OUTPUT_USER_ROOT", "VOICE_CONFIGURED_METADATA_MODE"}
        ),
        "client_env_accounting": "unescaped-projection-not-final-command-length",
        "windows_command_line_limit_utf16_units": 32768,
        "metadata_named_path_bindings_requested": True,
        "selected_path_utf16_units": utf16_units(indexed["CODEX_BAZEL_WINDOWS_PATH"]),
        "repeated_path_utf16_units_removed_projection": 3
        * (1 + utf16_units(indexed["CODEX_BAZEL_WINDOWS_PATH"])),
    }
    return child, metrics


def capture(command, destination, budget, timeout=45, *, env=None):
    cap = max(0, min(LIMIT, TOTAL - MANIFEST_RESERVE - budget[0]))
    result = capture_owned(command, destination, cap, timeout, env=env)
    budget[0] += result["retained_bytes"]
    return result


# Output is a closed vocabulary of observations, never excerpts or causal claims.
# Each category is OR across alternatives, AND across fixed literal needles.
# Co-occurrence is deliberately only an observation, with no ordering/cause claim.
FAILURE_PATTERNS = {
    "memory_signature": (
        (b"outofmemoryerror",),
        (b"out of memory",),
        (b"cannot allocate memory",),
        (b"native memory allocation", b"failed"),
    ),
    "paging_signature": ((b"paging file is too small",), (b"paging file", b"minimum")),
    "disk_full_signature": (
        (b"no space left on device",),
        (b"disk is full",),
        (b"not enough space on the disk",),
    ),
    "path_too_long_signature": (
        (b"filename or extension is too long",),
        (b"path too long",),
        (b"filename too long",),
    ),
    "symlink_signature": (
        (b"cannot create symbolic link",),
        (b"failed to create symbolic link",),
        (b"symlink", b"failed"),
        (b"symlink", b"error"),
    ),
    "lock_signature": (
        (b"failed to acquire", b"lock"),
        (b"cannot acquire", b"lock"),
        (b"another command", b"running"),
    ),
    "server_signature": (
        (b"server terminated abruptly",),
        (b"server crashed",),
        (b"unable to start", b"server"),
    ),
    "connect_signature": (
        (b"connection refused",),
        (b"failed to connect",),
        (b"could not connect",),
        (b"cannot connect",),
    ),
    "invalid_option_signature": (
        (b"unrecognized option",),
        (b"unrecognized vm option",),
        (b"unknown option",),
        (b"unknown startup option",),
        (b"invalid option",),
        (b"invalid flag",),
    ),
    "starlark_evaluation_signature": (
        (b"error evaluating starlark",),
        (b"starlark evaluation error",),
        (b"traceback (most recent call last):",),
    ),
    "formatter_signature": (
        (b"error evaluating", b"format"),
        (b"error evaluating", b"starlark expression"),
        (b"invalid", b"starlark", b"expression"),
        (b"formatter", b"failed"),
        (b"formatter", b"error"),
    ),
    "java_fatal_signature": (
        (b"a fatal error has been detected by the java runtime environment",),
        (b"error occurred during initialization of vm",),
        (b"could not create the java virtual machine",),
    ),
}
KNOWN_EXCEPTION_CLASSES = (
    "java.lang.OutOfMemoryError",
    "java.lang.IllegalStateException",
    "java.lang.IllegalArgumentException",
    "java.lang.NullPointerException",
    "java.lang.RuntimeException",
    "java.lang.ExceptionInInitializerError",
    "java.io.IOException",
    "java.nio.file.FileSystemException",
    "java.nio.file.AccessDeniedException",
    "java.nio.file.NoSuchFileException",
    "com.google.devtools.build.lib.util.AbruptExitException",
    "net.starlark.java.eval.EvalException",
)
KNOWN_OS_EXCEPTION_CODES = (0xC0000005, 0xC0000017, 0xC00000FD, 0xC0000409)


# Pinned Bazel 9.2.0 blaze.cc, blaze_util_windows.cc, native/windows/file.cc
# and util.cc literals. OS phrases are English FormatMessage observations only;
# absence may reflect localization. These never identify a cause or prove a site.
LINE_LITERALS = {
    "installation_symlink": b"failed to create installation symlink '",
    "symlink_directories": b"symlinkdirectories(",
    "junction_call": b"): createjunction: ",
    "createfilew": b"): createfilew(",
    "getfileinformationbyhandle": b"): getfileinformationbyhandle(",
    "deviceiocontrol": b"): deviceiocontrol(",
    "createjunction": b"): createjunction(",
    "junction_name_validation": b"expected an absolute windows path for junction_name",
    "junction_target_validation": b"expected an absolute windows path for junction_target",
    "junction_target_too_long": b"target path is too long",
    "os_memory_resources": b"not enough memory resources are available to process this command",
    "os_storage_operation": b"not enough storage is available to complete this operation",
    "out_of_memory": b"out of memory",
    "native_memory_allocation": b"native memory allocation",
    "failed": b"failed",
}

# Every positive output predicate requires at least one literal other than
# "failed" (which only qualifies native_memory_allocation). A shorter line
# therefore cannot affect any output. Derive the threshold from that vocabulary.
MIN_LINE_EVIDENCE_LENGTH = min(
    len(needle) for name, needle in LINE_LITERALS.items() if name != "failed"
)


def classify_lines(lowered):
    """One bounded line pass, fixed searches per nonempty line, fixed output.

    Keep whole-buffer observations distinct from same-line co-occurrence.
    Even exact literals can occur in help text or an arbitrary path/message.
    Only fixed booleans and two bounded claimed command lengths escape; line
    contents, source line numbers and command suffixes are never retained.
    """
    result = {
        "any_line_installation_symlink_literal": False,
        "fatal_installation_symlink_line": False,
        "junction_api_line": False,
        "fatal_command_line_too_long_line": False,
        "command_line_claimed_length": None,
        "command_line_claimed_limit": None,
        **{
            f"junction_{name}_line": False
            for name in (
                "createfilew",
                "getfileinformationbyhandle",
                "deviceiocontrol",
                "name_validation",
                "target_validation",
                "target_too_long",
            )
        },
        **{
            f"{relation}{name}_line": False
            for name in (
                "os_memory_resources",
                "os_storage_operation",
                "out_of_memory",
                "native_memory_allocation_failed",
            )
            for relation in ("", "fatal_", "junction_", "api_")
        },
    }
    # BytesIO iteration visits each LF-delimited line once, including a final
    # unterminated line. Short-line floods do no literal searches. CR is not a
    # cross-line gap: split CR-only input too, without building a list of lines.
    for lf_line in io.BytesIO(lowered):
        if len(lf_line) < MIN_LINE_EVIDENCE_LENGTH:
            continue
        for match in re.finditer(rb"[^\r\n]+", lf_line):
            if match.end() - match.start() < MIN_LINE_EVIDENCE_LENGTH:
                continue
            line = match[0]
            present = {name: needle in line for name, needle in LINE_LITERALS.items()}
            fatal = line.startswith(b"fatal:")
            # Bazel 9.2.0 blaze_util_windows.cc CreateCommandLine; the source
            # checks >=32768 although its fixed diagnostic prints " > ".
            # Anchor at the fatal message start. Never search into appended argv
            # for another prefix, and never capture or export the argv suffix.
            command_line_prefix = b"fatal: command line too long ("
            if line.startswith(command_line_prefix):
                result["fatal_command_line_too_long_line"] = True
                claimed = re.match(
                    rb"^fatal: command line too long \(([0-9]{1,8}) > ([0-9]{1,8})\): ",
                    line,
                )
                if (
                    claimed is not None
                    and result["command_line_claimed_length"] is None
                ):
                    result["command_line_claimed_length"] = int(claimed[1])
                    result["command_line_claimed_limit"] = int(claimed[2])
            junction = present["symlink_directories"] and present["junction_call"]
            api = b"error: " in line and any(
                present[name]
                for name in (
                    "createfilew",
                    "getfileinformationbyhandle",
                    "deviceiocontrol",
                    "createjunction",
                )
            )
            result["any_line_installation_symlink_literal"] |= present[
                "installation_symlink"
            ]
            result["fatal_installation_symlink_line"] |= (
                fatal and present["installation_symlink"]
            )
            result["junction_api_line"] |= junction
            for name in (
                "createfilew",
                "getfileinformationbyhandle",
                "deviceiocontrol",
            ):
                result[f"junction_{name}_line"] |= junction and present[name]
            for name in ("name_validation", "target_validation", "target_too_long"):
                result[f"junction_{name}_line"] |= (
                    junction
                    and present["createjunction"]
                    and present[f"junction_{name}"]
                )
            phrases = {
                name: present[name]
                for name in (
                    "os_memory_resources",
                    "os_storage_operation",
                    "out_of_memory",
                )
            }
            phrases["native_memory_allocation_failed"] = (
                present["native_memory_allocation"] and present["failed"]
            )
            for name, found in phrases.items():
                for relation, associated in (
                    ("", True),
                    ("fatal_", fatal),
                    ("junction_", junction),
                    ("api_", api),
                ):
                    result[f"{relation}{name}_line"] |= found and associated
    return result


# Bazel 9.2.0 (commit 8220c6198837d5c13d53fea211cf3282aa12408a):
# archive_utils.cc:248 and util/{logging,bazel_log_handler}.cc emit the USER
# extraction line. UiStateTracker.java:90,417..438,1179,1371,1392 and
# SkymeldUiStateTracker.java:84..103,131 emit these fixed progress prefixes;
# UiEventHandler.java:1093..1094 writes them as lines with --curses=no.
# Fetching detail can be suppressed by that short-version rendering. Timestamps,
# ANSI decoration and other spellings are deliberately not normalized/guessed.
PROGRESS_MARKERS = (
    "installation_extraction",
    "main_repository_mapping",
    "loading",
    "analysis",
    "fetch_detail",
)
PROGRESS_COUNT_LIMIT = 65535
PROGRESS_PREFIX = re.compile(
    rb"(?:\A|(?<=[\r\n]))(?:"
    rb"(?P<installation_extraction>Extracting Bazel installation\.\.\.)(?=[\r\n]|\Z)|"
    rb"(?P<main_repository_mapping>Computing main repo mapping: )|"
    rb"(?P<loading>Loading: )|"
    rb"(?P<analysis>Analyzing: )|"
    rb"(?P<fetch_detail>    Fetching ))"
)


def classify_progress(raw):
    """Count fixed line-prefix observations, never infer execution or progress.

    Arbitrary private text can spoof every marker; absence can reflect suppressed
    output. Counts are bounded line observations, not completed operations or
    elapsed-time measurements. No suffix, URL, label or captured value escapes.
    Fixed anchored literals make one bounded scan, including for many tiny lines.
    """
    counts = dict.fromkeys(PROGRESS_MARKERS, 0)
    for match in PROGRESS_PREFIX.finditer(raw, 0, min(len(raw), LIMIT)):
        name = match.lastgroup
        counts[name] = min(counts[name] + 1, PROGRESS_COUNT_LIMIT)
    return {
        "schema": "fixed-progress-prefix-observations-v1",
        "count_limit": PROGRESS_COUNT_LIMIT,
        "prefix_line_counts": counts,
    }


def classify_failure(raw):
    """Observe at most LIMIT private bytes; emit only selected constants.

    A matching signature may occur in arbitrary text, so it identifies evidence
    only. Neither a match nor exit 37 establishes an underlying failure cause.
    No captured group, decoded line, or arbitrary input reaches the result.
    """
    raw = raw[:LIMIT]
    lowered = raw.lower()
    # Fixed vocabulary bounds the number of linear literal searches regardless
    # of repeated prefixes. No suffix search restarts at each prefix occurrence.
    present = {
        needle: needle in lowered
        for alternatives in FAILURE_PATTERNS.values()
        for alternative in alternatives
        for needle in alternative
    }
    signatures = [
        name
        for name, alternatives in FAILURE_PATTERNS.items()
        if any(
            all(present[needle] for needle in alternative)
            for alternative in alternatives
        )
    ]
    exceptions = [
        name
        for name in KNOWN_EXCEPTION_CLASSES
        if re.search(rb"(?<![\w.$])" + re.escape(name.encode()) + rb"(?![\w.$])", raw)
    ]
    # Decimal Win32 codes require an explicit marker and are bounded to DWORD.
    # Conversion is bounded before int(); numbers in arbitrary text are ignored.
    win32_codes = sorted(
        {
            int(match[1])
            for match in re.finditer(
                rb"(?i)\b(?:win32(?: error)?|getlasterror|createprocess(?:w)? failed(?: with error)?)\s{0,16}[:=(]?\s{0,16}([0-9]{1,10})(?![0-9])",
                raw,
            )
            if int(match[1]) <= 0xFFFFFFFF
        }
    )[:16]
    os_codes = [
        code
        for code in KNOWN_OS_EXCEPTION_CODES
        if re.search(
            rb"(?i)(?<![0-9a-z])0x" + f"{code:08x}".encode() + rb"(?![0-9a-z])",
            raw,
        )
    ]
    error = bool(re.search(rb"(?m)^ERROR:", raw))
    fatal = bool(re.search(rb"(?m)^FATAL:", raw))
    return {
        "schema": "static-failure-signatures-v1",
        "evidence": "recognized-signatures"
        if any((signatures, exceptions, win32_codes, os_codes, error, fatal))
        else "unknown",
        "cause": "unknown",
        "line_evidence": classify_lines(lowered),
        "progress_observations": classify_progress(raw),
        "memory_alternatives": {
            "outofmemoryerror_token": present[b"outofmemoryerror"],
            "out_of_memory_phrase": present[b"out of memory"],
            "cannot_allocate_memory_phrase": present[b"cannot allocate memory"],
            "native_memory_allocation_and_failed_tokens": present[
                b"native memory allocation"
            ]
            and present[b"failed"],
        },
        # startup_options.cc 9.2.0 emits this ordinary JVM argument; observing
        # its embedded token alone is not an observed OutOfMemoryError event.
        "known_jvm_oom_option_marker": b"-xx:onoutofmemoryerror=touch " in lowered,
        "error_prefix": error,
        "fatal_prefix": fatal,
        "jvm_fatal_signature": "java_fatal_signature" in signatures,
        "signatures": signatures,
        "exception_classes": exceptions,
        "win32_decimal_codes": win32_codes,
        "known_os_exception_codes": os_codes,
    }


def failure_classification(output):
    # The owned capture enforces its own cap; also bound this independent read.
    with output.open("rb") as stream:
        return classify_failure(stream.read(LIMIT))


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def admit_query(query_deadline, timeout):
    # Admit only a FULL fixed window plus owned cleanup. Never shorten a query,
    # extend a deadline, retry or treat an unlaunched phase as accepted evidence.
    require(
        time.monotonic() + timeout + OWNED_CLEANUP_RESERVE_SECONDS <= query_deadline,
        "Insufficient global budget for complete configured query",
    )


def main():
    invocation_deadline = time.monotonic() + INVOCATION_SECONDS
    query_deadline = invocation_deadline - POSTPROCESSING_RESERVE_SECONDS
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
    parent_environment = dict(os.environ)
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
        child_environment, accounting = diagnostic_environment(
            parent_environment, query_root, args.expected_head, args.expected_tree
        )
        manifest["environment_accounting"] = accounting
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
            timeout = FIRST_PROVIDER_SECONDS if i == 0 else LATER_PROVIDER_SECONDS
            admit_query(query_deadline, timeout)
            result = capture(
                cmd, output, budget, timeout=timeout, env=child_environment
            )
            manifest["phases"].append(
                {
                    "phase": f"provider-{i}",
                    **result,
                    "failure_classification": failure_classification(output),
                }
            )
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
        admit_query(query_deadline, ACTIONS_SECONDS)
        result = capture(
            cmd, output, budget, timeout=ACTIONS_SECONDS, env=child_environment
        )
        manifest["phases"].append(
            {
                "phase": "actions",
                **result,
                "failure_classification": failure_classification(output),
            }
        )
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
        require(
            time.monotonic() <= invocation_deadline,
            "Configured metadata invocation deadline exhausted",
        )
        for name, value in payloads.items():
            (upload / name).write_bytes(value)
        manifest["status"] = "configured-graph-and-package-structure-validated"
    except EvidenceError as error:
        manifest["failure"] = str(error)
        if isinstance(error, RejectedWrapperLine):
            manifest["rejected_wrapper_line_shape"] = error.shape.copy()
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
        within_deadline = time.monotonic() <= invocation_deadline
        if not within_deadline:
            manifest["status"] = "failed-deadline"
            manifest["failure"] = "Configured metadata invocation deadline exhausted"
        (upload / "manifest.json").write_bytes(encode(manifest))
        require(within_deadline, "Configured metadata invocation deadline exhausted")
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
