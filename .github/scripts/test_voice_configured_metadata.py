"""Adversarial fixtures for configured evidence, without invoking Bazel."""

import copy
import json
from pathlib import Path
import ctypes as c
import os
import sys
import tempfile
import unittest
from unittest import mock

import voice_configured_metadata as m
import voice_metadata_owned_job as owned


def graph():
    return {
        "pathFragments": [
            {"id": 1, "label": "root"},
            {"id": 2, "label": "in", "parentId": 1},
            {"id": 3, "label": "out", "parentId": 1},
        ],
        "artifacts": [{"id": 10, "pathFragmentId": 2}, {"id": 11, "pathFragmentId": 3}],
        "depSetOfFiles": [
            {"id": 20, "directArtifactIds": [10]},
            {"id": 21, "transitiveDepSetIds": [20]},
        ],
        "targets": [{"id": 30, "label": "//fixture:tool", "ruleClassId": 40}],
        "ruleClasses": [{"id": 40, "name": "fixture"}],
        "configuration": [{"id": 50, "mnemonic": "fixture"}],
        "actions": [
            {
                "targetId": 30,
                "configurationId": 50,
                "mnemonic": "Fixture",
                "inputDepSetIds": [21],
                "outputIds": [11],
                "primaryOutputId": 11,
                "arguments": ["compiler.exe", "@root/params"],
                "paramFiles": [{"execPath": "root/params", "arguments": ["root/in"]}],
                "environmentVariables": [{"key": "PATH", "value": "private-value"}],
            }
        ],
    }


def packages():
    records = {
        label: {
            "label": label,
            "provider_keys": ["DefaultInfo"],
            "default": {
                "files": [],
                "executable": None,
                "default_runfiles": [],
                "data_runfiles": [],
            },
        }
        for label in m.LABELS
    }
    actions = []
    for raw_name, package_name in ((m.NAMES[1], m.NAMES[2]), (m.NAMES[3], m.NAMES[4])):
        raw, package = (records[m.PREFIX + name] for name in (raw_name, package_name))
        raw_exe, exe = raw_name + ".exe", package_name + "/tool.exe"
        dlls = [package_name + "/" + str(i) + ".dll" for i in range(25)]
        receipt = package_name + "/packaging.json"
        raw["default"]["executable"] = raw_exe
        raw["default"]["default_runfiles"] = ["upstream"]
        package["default"] = {
            "files": [exe],
            "executable": exe,
            "default_runfiles": dlls + [receipt, "upstream"],
            "data_runfiles": dlls + [receipt],
        }
        package["voice_package"] = {
            "raw_executable": raw_exe,
            "executable": exe,
            "dlls": dlls,
            "receipt": receipt,
        }
        actions.append(
            {
                "owner": package["label"],
                "mnemonic": "VoiceExecutablePackage",
                "inputs": [raw_exe],
                "outputs": [exe, receipt] + dlls,
            }
        )
    records[m.LABELS[0]]["default"]["executable"] = records[m.LABELS[2]][
        "voice_package"
    ]["executable"]
    return list(records.values()), actions


class ParsingTests(unittest.TestCase):
    def test_merged_wrapper_output_and_nested_braces(self):
        value = {"actions": [{"arguments": ["{literal}", "x"]}]}
        text = "BuildBuddy API key is not available; using local Bazel configuration.\n"
        text += json.dumps(value, indent=2) + "\nINFO: done\n"
        self.assertEqual(m.parse_json(text), value)

    def test_truncated_malformed_multiple_and_error_output(self):
        for text in (
            '{"actions": [',
            "{bad}",
            "{}\n{}",
            "ERROR: formatter failed\n{}",
            "{}\nERROR: action failed",
            "notice containing { not JSON\n{}",
        ):
            with self.subTest(text=text), self.assertRaises(m.EvidenceError):
                m.parse_json(text)

    def test_provider_sentinel_exactly_once_and_correct_identity(self):
        record = packages()[0][0]
        text = m.SENTINEL + json.dumps(record) + "\nINFO: done"
        self.assertEqual(m.parse_provider(text, record["label"]), record)
        for wrong in (
            text + "\n" + text,
            "ERROR: failed\n" + text,
            m.SENTINEL + "{}",
            m.SENTINEL + "{",
        ):
            with self.subTest(text=wrong), self.assertRaises(m.EvidenceError):
                m.parse_provider(wrong, record["label"])


class GraphTests(unittest.TestCase):
    def test_complete_graph_paths_and_arguments_retained_env_values_removed(self):
        raw = graph()
        sanitized, resolved = m.graph_metadata(raw)
        self.assertEqual(resolved[0]["inputs"], ["root/in"])
        self.assertEqual(resolved[0]["outputs"], ["root/out"])
        self.assertEqual(
            sanitized["actions"][0]["arguments"], raw["actions"][0]["arguments"]
        )
        self.assertEqual(sanitized["artifacts"], raw["artifacts"])
        self.assertNotIn("private-value", json.dumps(sanitized))
        self.assertIn("private-value", json.dumps(raw))

    def test_generated_text_and_substitution_keys_values_are_irreversibly_redacted(
        self,
    ):
        for field in ("templateContent", "fileContents", "unresolvedSymlinkTarget"):
            for text in (
                "PATH=private-value\n",
                "private-value",
                "unrecognized-data",
                "Path=private-value",
            ):
                raw = graph()
                raw["actions"][0][field] = text
                sanitized, _ = m.graph_metadata(raw)
                self.assertEqual(sanitized["actions"][0][field], "<redacted>")
                self.assertNotIn("private-value", json.dumps(sanitized))
        raw = graph()
        raw["actions"][0]["substitutions"] = [
            {"key": "private-value", "value": "other-value"}
        ]
        sanitized, _ = m.graph_metadata(raw)
        self.assertEqual(
            sanitized["actions"][0]["substitutions"],
            [{"key": "<redacted>", "value": "<redacted>"}],
        )
        self.assertNotIn("private-value", json.dumps(sanitized))

    def test_mixed_case_assignments_fail_in_arguments_and_parameter_files(self):
        for assignment in (
            "Path=private-value",
            "SystemRoot=private-value",
            "path=private-value",
            "_private=private-value",
            "a1=private-value",
        ):
            for param in (False, True):
                raw = graph()
                args = (
                    raw["actions"][0]["paramFiles"][0]["arguments"]
                    if param
                    else raw["actions"][0]["arguments"]
                )
                args.append(assignment)
                with (
                    self.subTest(assignment=assignment, param=param),
                    self.assertRaises(m.EvidenceError),
                ):
                    m.graph_metadata(raw)

    def test_missing_references_duplicates_and_cycles_fail(self):
        mutations = [
            lambda g: g["artifacts"][0].update(pathFragmentId=999),
            lambda g: g["actions"][0].update(outputIds=[999]),
            lambda g: g["depSetOfFiles"][0].update(directArtifactIds=[999]),
            lambda g: g["depSetOfFiles"][0].update(transitiveDepSetIds=[21]),
            lambda g: g["pathFragments"][0].update(parentId=2),
            lambda g: g["targets"][0].update(ruleClassId=999),
            lambda g: g["actions"][0].update(configurationId=999),
            lambda g: g["actions"][0].update(aspectDescriptorIds=[999]),
            lambda g: g["artifacts"].append(copy.deepcopy(g["artifacts"][0])),
        ]
        for mutate in mutations:
            value = graph()
            mutate(value)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.graph_metadata(value)

    def test_provider_environment_values_are_private_and_equalities_use_raw_values(
        self,
    ):
        records, actions = packages()
        for record in records[1:3]:
            record["run_environment"] = {
                "environment": {"PATH": "private-path"},
                "inherited_environment_names": [],
            }
        sanitized = m.sanitize_provider(records[1])
        self.assertNotIn("private-path", json.dumps(sanitized))
        self.assertEqual(len(m.package_proof(records, actions)), 2)
        records[2]["run_environment"]["environment"]["PATH"] = "different-private-path"
        with self.assertRaises(m.EvidenceError):
            m.package_proof(records, actions)
        records[1]["run_environment"]["environment"]["UNREVIEWED"] = "secret-value"
        with self.assertRaises(m.EvidenceError):
            m.sanitize_provider(records[1])

    def test_unknown_fields_env_and_secret_arguments_fail(self):
        mutations = [
            lambda g: g.update(unknown=[]),
            lambda g: g["actions"][0].update(unknown="x"),
            lambda g: g["actions"][0]["environmentVariables"].append(
                {"key": "UNREVIEWED", "value": "x"}
            ),
            lambda g: g["actions"][0]["arguments"].append(
                "--remote_header=authorization=x"
            ),
            lambda g: g["actions"][0]["paramFiles"][0]["arguments"].append(
                "--env=PATH=private"
            ),
        ]
        for mutate in mutations:
            value = graph()
            mutate(value)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.graph_metadata(value)

    def test_package_counts_bind_to_actual_provider_paths(self):
        records, actions = packages()
        self.assertEqual(len(m.package_proof(records, actions)), 2)
        for mutate in (
            lambda r, a: a.append(copy.deepcopy(a[0])),
            lambda r, a: a[0]["outputs"].pop(),
            lambda r, a: a[0]["outputs"].__setitem__(0, "unrelated.exe"),
            lambda r, a: a[0]["inputs"].clear(),
            lambda r, a: r[2]["default"]["default_runfiles"].remove("upstream"),
            lambda r, a: r[1].update(run_environment={"environment": {"PATH": "raw"}}),
        ):
            r, a = copy.deepcopy((records, actions))
            mutate(r, a)
            with self.subTest(mutation=mutate), self.assertRaises(m.EvidenceError):
                m.package_proof(r, a)


class FakeAPI:
    """Pure Win32 mock: pipes carry fixtures; no process is created."""

    def __init__(self, payload=b"fixture", code=7, fail_assign=False, wait=0):
        self.payload, self.code = payload, code
        self.fail_assign, self.wait = fail_assign, wait
        self.events = []

    def oshandle(self, fd):
        return fd

    def __getattr__(self, name):
        def call(*args):
            self.events.append(name)
            if name == "CreateJobObjectW":
                return 1001
            if name == "InitializeProcThreadAttributeList":
                c.cast(args[3], c.POINTER(c.c_size_t))[0] = 64
            if name == "CreateProcessW":
                pi = c.cast(args[-1], c.POINTER(owned.PI)).contents
                pi.process, pi.thread, pi.pid = 1002, 1003, 1004
                si = c.cast(args[-2], c.POINTER(owned.SIEX)).contents
                import os

                os.write(si.startup.hStdOutput, self.payload)
            if name == "GetProcessTimes":
                c.cast(args[1], c.POINTER(owned.w.FILETIME)).contents.dwLowDateTime = 42
            if name == "AssignProcessToJobObject":
                return not self.fail_assign
            if name == "IsProcessInJob":
                c.cast(args[2], c.POINTER(owned.w.BOOL))[0] = True
            if name == "WaitForSingleObject":
                return self.wait if args[1] == 25 else 0
            if name == "GetExitCodeProcess":
                c.cast(args[1], c.POINTER(owned.w.DWORD))[0] = self.code
            return 1

        return call


class FailureClassificationTests(unittest.TestCase):
    def test_closed_signature_fixtures(self):
        fixtures = {
            "memory_signature": b"java.lang.OutOfMemoryError: private",
            "paging_signature": b"The paging file is too small",
            "disk_full_signature": b"No space left on device",
            "path_too_long_signature": b"filename or extension is too long",
            "symlink_signature": b"cannot create symbolic link private",
            "lock_signature": b"failed to acquire private lock",
            "server_signature": b"Server terminated abruptly",
            "connect_signature": b"Connection refused",
            "invalid_option_signature": b"Unrecognized VM option private",
            "starlark_evaluation_signature": b"Error evaluating Starlark",
            "formatter_signature": b"formatter failed private",
            "java_fatal_signature": b"A fatal error has been detected by the Java Runtime Environment",
        }
        self.assertEqual(set(fixtures), set(m.FAILURE_PATTERNS))
        for name, raw in fixtures.items():
            with self.subTest(name=name):
                result = m.classify_failure(raw)
                self.assertIn(name, result["signatures"])
                self.assertEqual(result["cause"], "unknown")
                self.assertEqual(
                    result["jvm_fatal_signature"], name == "java_fatal_signature"
                )
        for name in m.KNOWN_EXCEPTION_CLASSES:
            self.assertEqual(
                m.classify_failure(name.encode())["exception_classes"], [name]
            )
        for code in m.KNOWN_OS_EXCEPTION_CODES:
            self.assertEqual(
                m.classify_failure(f"0x{code:08X}".encode())[
                    "known_os_exception_codes"
                ],
                [code],
            )
        result = m.classify_failure(
            b"ERROR: private\nFATAL: private\nWin32 error: 1455\nGetLastError=112"
        )
        self.assertTrue(result["error_prefix"])
        self.assertTrue(result["fatal_prefix"])
        self.assertEqual(result["win32_decimal_codes"], [112, 1455])

    def test_every_literal_alternative_and_missing_wildcard_suffix(self):
        for name, alternatives in m.FAILURE_PATTERNS.items():
            for alternative in alternatives:
                with self.subTest(name=name, alternative=alternative):
                    raw = b" private-gap ".join(alternative)
                    self.assertIn(name, m.classify_failure(raw)["signatures"])
        # Every former wildcard prefix without its required suffix occurs many
        # times on one line. Exercise the real classifier at the unchanged cap.
        prefixes = (
            b"native memory allocation paging file symlink another command unable to start formatter ",
            b"failed to acquire cannot acquire error evaluating invalid starlark ",
        )
        for prefix in prefixes:
            raw = (prefix * (m.LIMIT // len(prefix) + 1))[: m.LIMIT]
            self.assertEqual(len(raw), 32 * 1024 * 1024)
            self.assertEqual(m.classify_failure(raw), m.classify_failure(b""))
        # Explicit numeric marker followed by a long missing-code gap must also
        # terminate; bounded separators prevent whitespace backtracking growth.
        raw = b"Win32 error:" + b" " * (m.LIMIT - len(b"Win32 error:"))
        self.assertEqual(m.classify_failure(raw), m.classify_failure(b""))

    def test_line_sites_phrases_and_private_material(self):
        private = b"https://private.invalid/PRIVATE_TOKEN C:/private/PASSWORD"
        prefix = (
            b"ERROR: SymlinkDirectories("
            + private
            + b"): CreateJunction: ERROR: file.cc(123): "
        )
        for api in (b"CreateFileW", b"GetFileInformationByHandle", b"DeviceIoControl"):
            raw = (
                prefix
                + api
                + b"("
                + private
                + b"): Not enough memory resources are available to process this command"
            )
            result = m.classify_failure(raw)
            evidence = result["line_evidence"]
            self.assertTrue(evidence["junction_api_line"])
            self.assertTrue(evidence["junction_" + api.decode().lower() + "_line"])
            self.assertTrue(evidence["junction_os_memory_resources_line"])
            self.assertTrue(evidence["api_os_memory_resources_line"])
            self.assertFalse(evidence["fatal_os_memory_resources_line"])
            self.assertEqual(result["cause"], "unknown")
            self.assertNotIn("PRIVATE_TOKEN", json.dumps(result))
            self.assertNotIn("https://", json.dumps(result))
        for name, phrase in (
            ("name_validation", b"expected an absolute Windows path for junction_name"),
            (
                "target_validation",
                b"expected an absolute Windows path for junction_target",
            ),
            ("target_too_long", b"target path is too long"),
        ):
            evidence = m.classify_failure(
                prefix + b"CreateJunction(" + private + b"): " + phrase
            )["line_evidence"]
            self.assertTrue(evidence["junction_" + name + "_line"])
        fatal = (
            b"FATAL: failed to create installation symlink '"
            + private
            + b"': Not enough storage is available to complete this operation"
        )
        evidence = m.classify_failure(fatal)["line_evidence"]
        self.assertTrue(evidence["fatal_installation_symlink_line"])
        self.assertTrue(evidence["fatal_os_storage_operation_line"])
        self.assertFalse(evidence["junction_os_storage_operation_line"])

    def test_each_memory_phrase_has_independent_same_line_relations(self):
        prefix = b"FATAL: SymlinkDirectories(private): CreateJunction: ERROR: file.cc(1): CreateFileW(private): "
        for name, phrase in (
            (
                "os_memory_resources",
                b"not enough memory resources are available to process this command",
            ),
            (
                "os_storage_operation",
                b"not enough storage is available to complete this operation",
            ),
            ("out_of_memory", b"out of memory"),
            (
                "native_memory_allocation_failed",
                b"native memory allocation private failed",
            ),
        ):
            evidence = m.classify_failure(prefix + phrase)["line_evidence"]
            for relation in ("", "fatal_", "junction_", "api_"):
                self.assertTrue(evidence[relation + name + "_line"])
            self.assertFalse(evidence["fatal_installation_symlink_line"])

    def test_line_relations_do_not_cross_line_boundaries(self):
        for separator in (b"\n", b"\r\n", b"\r"):
            raw = separator.join(
                (
                    b"FATAL: unrelated",
                    b"failed to create installation symlink 'private'",
                    b"SymlinkDirectories(private",
                    b"): CreateJunction: ERROR: file.cc(1): CreateFileW(private): unrelated",
                    b"not enough memory resources are available to process this command",
                    b"not enough storage is available to complete this operation",
                    b"native memory allocation",
                    b"failed",
                )
            )
            evidence = m.classify_failure(raw)["line_evidence"]
            self.assertTrue(evidence["any_line_installation_symlink_literal"])
            self.assertTrue(evidence["os_memory_resources_line"])
            self.assertFalse(evidence["fatal_installation_symlink_line"])
            self.assertFalse(evidence["junction_api_line"])
            self.assertFalse(evidence["native_memory_allocation_failed_line"])
            for relation in ("fatal_", "junction_", "api_"):
                self.assertFalse(evidence[relation + "os_memory_resources_line"])
        # Arbitrary help text can reproduce the exact constants. Record only
        # literal observations; never claim that it identifies an actual site.
        result = m.classify_failure(
            b"help: SymlinkDirectories(x): CreateJunction: documentation"
        )
        self.assertTrue(result["line_evidence"]["junction_api_line"])
        self.assertEqual(result["cause"], "unknown")
        evidence = m.classify_failure(
            b"FATAL: memoire insuffisante CreateFileW(private)"
        )["line_evidence"]
        self.assertFalse(evidence["os_memory_resources_line"])
        self.assertFalse(evidence["api_os_memory_resources_line"])

    def test_line_classifier_large_linecount_and_unterminated_prefixes(self):
        unknown = m.classify_lines(b"")
        self.assertEqual(m.classify_lines(b"\n" * m.LIMIT), unknown)
        prefix = b"SymlinkDirectories( CreateFileW( native memory allocation "
        raw = (prefix * (m.LIMIT // len(prefix) + 1))[: m.LIMIT]
        self.assertEqual(len(raw), 32 * 1024 * 1024)
        self.assertEqual(m.classify_lines(raw.lower()), unknown)

    def test_full_limit_nonempty_tiny_line_floods_and_minimum_boundary(self):
        self.assertEqual(m.MIN_LINE_EVIDENCE_LENGTH, len(b"out of memory"))
        self.assertEqual(m.MIN_LINE_EVIDENCE_LENGTH, 13)
        unknown = m.classify_lines(b"")
        for separator in (b"\n", b"\r\n", b"\r"):
            with self.subTest(separator=separator):
                prefix = b"a" + separator
                raw = (prefix * (m.LIMIT // len(prefix) + 1))[: m.LIMIT]
                self.assertEqual(len(raw), 32 * 1024 * 1024)
                self.assertEqual(m.classify_lines(raw), unknown)
                # Exactly the minimum length, including an unterminated final
                # line, must still produce the shortest positive observation.
                for raw in (
                    b"out of memory",
                    b"out of memory" + separator,
                    b"a" + separator + b"out of memory",
                ):
                    evidence = m.classify_lines(raw)
                    self.assertTrue(evidence["out_of_memory_line"])
                    self.assertFalse(evidence["fatal_out_of_memory_line"])

    def test_fatal_command_length_grammar_never_exports_argv(self):
        suffix = b"C:/PRIVATE_PASSWORD https://private.invalid/?token=PRIVATE_TOKEN --authorization=PRIVATE_AUTH"
        for length in (32768, 45000, 99999999):
            raw = f"FATAL: Command line too long ({length} > 32768): ".encode() + suffix
            result = m.classify_failure(raw)
            evidence = result["line_evidence"]
            self.assertTrue(evidence["fatal_command_line_too_long_line"])
            self.assertEqual(evidence["command_line_claimed_length"], length)
            self.assertEqual(evidence["command_line_claimed_limit"], 32768)
            self.assertEqual(result["cause"], "unknown")
            self.assertNotIn("PRIVATE_", json.dumps(result))
            self.assertNotIn("https://", json.dumps(result))
        for raw in (
            b"FATAL: Command line too long (999999999 > 32768): private",
            b"FATAL: Command line too long (45000 > 999999999): private",
            b"FATAL: Command line too long (-1 > 32768): private",
            b"FATAL: Command line too long (45000>32768): private",
            b"FATAL: Command line too long (45000 > 32768):private",
            b"FATAL: unrelated Command line too long (45000 > 32768): private",
            b"help: FATAL: Command line too long (45000 > 32768): private",
            b"FATAL: Command line too long (45000 >\n32768): private",
            b"FATAL: unrelated\nCommand line too long (45000 > 32768): private",
        ):
            with self.subTest(raw=raw):
                evidence = m.classify_failure(raw)["line_evidence"]
                self.assertIsNone(evidence["command_line_claimed_length"])
                self.assertIsNone(evidence["command_line_claimed_limit"])
        # Oversized digits and repeated near-prefixes on a full-limit line must
        # not cause restarting searches or capture anything from private argv.
        prefix = b"FATAL: Command line too long (999999999 > 32768): private "
        raw = (prefix * (m.LIMIT // len(prefix) + 1))[: m.LIMIT]
        evidence = m.classify_lines(raw.lower())
        self.assertTrue(evidence["fatal_command_line_too_long_line"])
        self.assertIsNone(evidence["command_line_claimed_length"])
        self.assertIsNone(evidence["command_line_claimed_limit"])

    def test_memory_alternatives_and_known_normal_jvm_option(self):
        fixtures = {
            "outofmemoryerror_token": b"OutOfMemoryError",
            "out_of_memory_phrase": b"out of memory",
            "cannot_allocate_memory_phrase": b"cannot allocate memory",
            "native_memory_allocation_and_failed_tokens": b"native memory allocation private failed",
        }
        for name, raw in fixtures.items():
            result = m.classify_failure(raw)
            self.assertEqual(
                result["memory_alternatives"], {key: key == name for key in fixtures}
            )
            self.assertFalse(result["known_jvm_oom_option_marker"])
        result = m.classify_failure(b"-XX:OnOutOfMemoryError=touch C:/PRIVATE_PATH")
        self.assertTrue(result["known_jvm_oom_option_marker"])
        self.assertTrue(result["memory_alternatives"]["outofmemoryerror_token"])
        self.assertEqual(result["exception_classes"], [])
        self.assertEqual(result["cause"], "unknown")
        self.assertNotIn("PRIVATE_PATH", json.dumps(result))

    def test_arbitrary_bytes_and_secret_text_cannot_escape(self):
        import random

        rng = random.Random(62)
        unknown = m.classify_failure(b"")
        for _ in range(100):
            secret = "PRIVATE_" + rng.randbytes(32).hex()
            raw = rng.randbytes(1000) + secret.encode()
            result = m.classify_failure(raw)
            self.assertNotIn(secret, json.dumps(result))
            raw += (
                b"\nERROR: "
                + secret.encode()
                + b"\njava.lang.OutOfMemoryError: "
                + secret.encode()
            )
            result = m.classify_failure(raw)
            self.assertNotIn(secret, json.dumps(result))
            self.assertEqual(
                result["exception_classes"], ["java.lang.OutOfMemoryError"]
            )
        self.assertEqual(unknown["evidence"], "unknown")
        result = m.classify_failure(
            b"Win32 error: 4294967296 Win32 error: 99999999999999999 1455 0xDEADBEEF java.lang.PrivateException"
        )
        self.assertEqual(result, unknown)
        self.assertEqual(m.classify_failure(b"x" * m.LIMIT + b"ERROR: hidden"), unknown)
        self.assertEqual(
            m.classify_failure(b"java.lang.OutOfMemoryErrorPrivate")[
                "exception_classes"
            ],
            [],
        )


class CaptureTests(unittest.TestCase):
    def test_main_exit37_records_only_accounting_and_safe_classification(self):
        secret = b"PRIVATE_NEVER_UPLOAD_62"
        raw = b"ERROR: " + secret + b"\n" + b"x" * (45467 - len(secret) - 8)
        self.assertEqual(len(raw), 45467)
        accounting = {
            "exit_code": 37,
            "observed_bytes": 45467,
            "retained_bytes": 45467,
            "oversize": False,
            "timeout": False,
            "assigned_before_resume": True,
            "reader_closed": True,
            "active_job_processes_after_cleanup": 0,
        }

        def capture(command, output, budget, timeout=45):
            output.write_bytes(raw)
            return accounting.copy()

        with tempfile.TemporaryDirectory() as directory:
            shell = Path(directory) / "bash.exe"
            shell.touch()
            root = Path(directory) / "evidence"
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "capture",
                        "--git-bash",
                        str(shell),
                        "--root",
                        str(root),
                        "--expected-head",
                        "head",
                        "--expected-tree",
                        "tree",
                    ],
                ),
                mock.patch.dict(
                    os.environ,
                    {"RUNNER_TEMP": directory, "PROCESSOR_ARCHITECTURE": "AMD64"},
                ),
                mock.patch.object(
                    m,
                    "git",
                    side_effect=lambda *args: (
                        "head"
                        if args == ("rev-parse", "HEAD")
                        else "tree"
                        if args == ("rev-parse", "HEAD^{tree}")
                        else ""
                    ),
                ),
                mock.patch.object(m, "capture", side_effect=capture) as captured,
                mock.patch.object(m, "parse_provider") as parsed,
                mock.patch.object(m, "parse_json") as parsed_graph,
            ):
                with self.assertRaisesRegex(m.EvidenceError, "Provider capture failed"):
                    m.main()
            captured.assert_called_once()
            parsed.assert_not_called()
            parsed_graph.assert_not_called()
            self.assertEqual(
                [p.name for p in (root / "upload").iterdir()], ["manifest.json"]
            )
            uploaded = (root / "upload" / "manifest.json").read_bytes()
            self.assertNotIn(secret, uploaded)
            manifest = json.loads(uploaded)
            self.assertEqual(manifest["status"], "failed")
            self.assertTrue(manifest["source_unchanged"])
            self.assertEqual(
                manifest["phases"],
                [
                    {
                        "phase": "provider-0",
                        **accounting,
                        "failure_classification": m.classify_failure(raw),
                    }
                ],
            )
            self.assertEqual((root / "private" / "provider-0.raw").read_bytes(), raw)

    def test_main_emits_batch_queries_with_absolute_shell_and_unique_private_root(self):
        records = {p["label"]: p for p in packages()[0]}
        seen = []

        def capture(command, output, budget, timeout=45):
            seen.append((command, timeout, os.environ["BAZEL_OUTPUT_USER_ROOT"]))
            if "cquery" in command:
                output.write_text(m.SENTINEL + json.dumps(records[command[-1]]))
            else:
                output.write_text(json.dumps(graph()))
            return {"oversize": False, "timeout": False, "exit_code": 0}

        with tempfile.TemporaryDirectory() as directory:
            shell = Path(directory) / "bash.exe"
            shell.touch()
            for attempt in range(2):
                with (
                    mock.patch.object(
                        sys,
                        "argv",
                        [
                            "capture",
                            "--git-bash",
                            str(shell),
                            "--root",
                            str(Path(directory) / str(attempt)),
                            "--expected-head",
                            "head",
                            "--expected-tree",
                            "tree",
                        ],
                    ),
                    mock.patch.dict(
                        os.environ,
                        {
                            "RUNNER_TEMP": directory,
                            "PROCESSOR_ARCHITECTURE": "AMD64",
                            "BAZEL_OUTPUT_USER_ROOT": "ordinary-runtime-root",
                        },
                    ),
                    mock.patch.object(
                        m,
                        "git",
                        side_effect=lambda *args: (
                            "head"
                            if args == ("rev-parse", "HEAD")
                            else "tree"
                            if args == ("rev-parse", "HEAD^{tree}")
                            else ""
                        ),
                    ),
                    mock.patch.object(m, "capture", side_effect=capture),
                    mock.patch.object(m, "package_proof", return_value=[]),
                    mock.patch("builtins.print"),
                ):
                    m.main()
            self.assertEqual(len(seen), 16)
            self.assertNotEqual(seen[0][2], seen[8][2])
            for command, timeout, root in seen:
                self.assertEqual(command[0], str(shell))
                query = "cquery" if "cquery" in command else "aquery"
                self.assertEqual(command[command.index(query) - 1], "--batch")
                self.assertEqual(timeout, 45 if query == "cquery" else 90)
                self.assertTrue(Path(root).is_relative_to(directory))
                self.assertNotEqual(root, "ordinary-runtime-root")

    def capture(self, api, cap=64, timeout=60):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "raw"
            result = owned.capture_owned(["qualified.exe"], path, cap, timeout, api=api)
            return result, path.read_bytes()

    def test_owned_identity_assignment_precedes_execution_and_cleanup_is_joined(self):
        api = FakeAPI()
        result, data = self.capture(api)
        self.assertEqual(data, b"fixture")
        self.assertEqual(result["exit_code"], 7)
        self.assertEqual(result["root_identity"]["creation_filetime"], 42)
        self.assertTrue(result["reader_closed"])
        self.assertEqual(result["active_job_processes_after_cleanup"], 0)
        self.assertLess(
            api.events.index("AssignProcessToJobObject"),
            api.events.index("ResumeThread"),
        )
        self.assertLess(
            api.events.index("TerminateJobObject"),
            api.events.index("QueryInformationJobObject"),
        )
        self.assertNotIn("TerminateProcess", api.events)

    def test_failed_assignment_never_resumes_and_terminates_only_retained_root(self):
        api = FakeAPI(fail_assign=True)
        with self.assertRaises(OSError):
            self.capture(api)
        self.assertNotIn("ResumeThread", api.events)
        self.assertIn("TerminateProcess", api.events)
        self.assertNotIn("TerminateJobObject", api.events)

    def test_cap_and_timeout_fail_without_timer_or_pid_lookup(self):
        result, data = self.capture(FakeAPI(payload=b"x" * 1000))
        self.assertTrue(result["oversize"])
        self.assertLessEqual(len(data), 64)
        result, _ = self.capture(FakeAPI(wait=258), timeout=-1)
        self.assertTrue(result["timeout"])
        self.assertIsNone(result["exit_code"])

    def test_total_budget_is_passed_to_owned_capture(self):
        with mock.patch.object(
            m, "capture_owned", return_value={"retained_bytes": 2}
        ) as capture:
            budget = [m.TOTAL - m.MANIFEST_RESERVE - 3]
            m.capture(["qualified.exe"], Path("unused"), budget)
            self.assertEqual(capture.call_args.args[2:], (3, 45))
            self.assertEqual(budget[0], m.TOTAL - m.MANIFEST_RESERVE - 1)


if __name__ == "__main__":
    unittest.main()
