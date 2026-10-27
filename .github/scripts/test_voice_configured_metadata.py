"""Adversarial fixtures for configured evidence, without invoking Bazel."""

import copy
import io
import json
from pathlib import Path
import ctypes as c
import os
import sys
import tempfile
import unittest
from unittest import mock

import run_bazel_with_buildbuddy as wrapper
import voice_configured_metadata as m
import voice_metadata_owned_job as owned


def diagnostic_parent(directory):
    env = {name: "fixture-" + name for name in m.REQUIRED_CHILD_ENV}
    env.update(
        {
            "RUNNER_TEMP": directory,
            "RUNNER_OS": "Windows",
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": "CompleteDotTech/codex-subagents-multi-provider",
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_JOB": "voice-runtime-diagnostic",
            "PROCESSOR_ARCHITECTURE": "AMD64",
            "VOICE_WINDOWS_HOST_ARCH": "AMD64",
            "SYSTEMROOT": "C:/Windows",
            "VOICE_WINDOWS_SYSTEM_ROOT": "C:/Windows",
            "VOICE_DIAGNOSTIC_HEAD": "head",
            "VOICE_DIAGNOSTIC_TREE": "tree",
            "GITHUB_SHA": "head",
        }
    )
    return env


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

    def test_exact_extraction_line_frames_provider_record(self):
        record = packages()[0][0]
        for separator in ("\n", "\r\n"):
            text = separator.join(
                (
                    "Extracting Bazel installation...",
                    m.SENTINEL + json.dumps(record),
                    "INFO: done",
                )
            )
            self.assertEqual(m.parse_provider(text, record["label"]), record)

    def test_exact_extraction_line_frames_action_json(self):
        value = graph()
        for separator in ("\n", "\r\n"):
            text = separator.join(
                (
                    "Extracting Bazel installation...",
                    json.dumps(value),
                    "INFO: done",
                )
            )
            self.assertEqual(m.parse_json(text), value)

    def test_extraction_allowance_rejects_variants_and_unknown_lines(self):
        record = packages()[0][0]
        for line in (
            "prefix Extracting Bazel installation...",
            "Extracting Bazel installation... PRIVATE",
            " Extracting Bazel installation...",
            "Extracting Bazel installation... ",
            "Extracting Bazel installation....",
            "Extracting Bazel installation..",
            "extracting Bazel installation...",
            "Extracting Other installation...",
            "PRIVATE unknown wrapper output",
        ):
            for before in (True, False):
                with self.subTest(line=line, before=before):
                    action = json.dumps(graph())
                    provider = m.SENTINEL + json.dumps(record)

                    def frame(payload):
                        return (
                            (line + "\n" + payload)
                            if before
                            else (payload + "\n" + line)
                        )

                    with self.assertRaises(m.EvidenceError):
                        m.parse_json(frame(action))
                    with self.assertRaises(m.EvidenceError):
                        m.parse_provider(frame(provider), record["label"])

    def test_extraction_line_preserves_duplicate_and_malformed_guards(self):
        line = "Extracting Bazel installation...\n"
        record = packages()[0][0]
        for body in ("{}\n{}", '{"a":1,"a":2}', '{"actions": [', "{bad}"):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_json(line + body)
        provider = m.SENTINEL + json.dumps(record)
        for body in (
            provider + "\n" + provider,
            m.SENTINEL + '{"label":"x","label":"x"}',
            m.SENTINEL + "{",
        ):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_provider(line + body, record["label"])


class ActualWrapperFramingTests(unittest.TestCase):
    def keyless_wrapper_stderr(self, query):
        env = {"CODEX_BAZEL_BIN": "fixture-bazel"}
        command = wrapper.bazel_command(query, env=env)
        self.assertIsNone(wrapper.remote_config([query], env))
        stderr = io.StringIO()
        with (
            mock.patch.dict(os.environ, env, clear=True),
            mock.patch.object(sys, "argv", ["wrapper", query]),
            mock.patch.object(sys, "stderr", stderr),
            mock.patch.object(
                wrapper.subprocess,
                "run",
                return_value=wrapper.subprocess.CompletedProcess(command, 0),
            ) as run,
            mock.patch.object(
                wrapper.os, "execvp", side_effect=SystemExit(0)
            ) as execute,
            self.assertRaises(SystemExit) as raised,
        ):
            wrapper.main()
        self.assertEqual(raised.exception.code, 0)
        if os.name == "nt":
            run.assert_called_once_with(command, check=False)
            execute.assert_not_called()
        else:
            execute.assert_called_once_with(command[0], command)
            run.assert_not_called()
        self.assertEqual(
            stderr.getvalue(),
            "BuildBuddy key unavailable; using local Bazel configuration.\n",
        )
        return stderr.getvalue()

    def test_real_keyless_wrapper_line_frames_provider_record(self):
        record = packages()[0][0]
        text = self.keyless_wrapper_stderr("cquery") + m.SENTINEL + json.dumps(record)
        self.assertEqual(m.parse_provider(text, record["label"]), record)

    def test_real_keyless_wrapper_line_frames_action_json(self):
        value = graph()
        text = self.keyless_wrapper_stderr("aquery") + json.dumps(value)
        self.assertEqual(m.parse_json(text), value)

    def test_python_wrapper_allowance_is_exact_complete_line(self):
        record = packages()[0][0]
        exact = self.keyless_wrapper_stderr("cquery").rstrip("\n")
        for line in (
            "prefix " + exact,
            exact + " PRIVATE_SUFFIX",
            " " + exact,
            exact + " ",
            exact.lower(),
            exact[:-1],
            "UNKNOWN_PRIVATE_WRAPPER_LINE",
        ):
            for before in (True, False):
                with self.subTest(line=line, before=before):

                    def frame(payload):
                        return (
                            line + "\n" + payload if before else payload + "\n" + line
                        )

                    with self.assertRaises(m.EvidenceError):
                        m.parse_json(frame(json.dumps(graph())))
                    with self.assertRaises(m.EvidenceError):
                        m.parse_provider(
                            frame(m.SENTINEL + json.dumps(record)), record["label"]
                        )

    def test_real_wrapper_line_preserves_duplicate_and_malformed_guards(self):
        line = self.keyless_wrapper_stderr("cquery")
        record = packages()[0][0]
        for body in ("{}\n{}", '{"a":1,"a":2}', '{"actions": [', "{bad}"):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_json(line + body)
        provider = m.SENTINEL + json.dumps(record)
        for body in (
            provider + "\n" + provider,
            m.SENTINEL + '{"label":"x","label":"x"}',
            m.SENTINEL + "{",
        ):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_provider(line + body, record["label"])


class JVMWarningFramingTests(unittest.TestCase):
    # Exact public line observed in the owned LOCALNATIVE capture and grounded
    # in OpenJDK 25.0.2 arguments.cpp/debug.cpp. Never allow a warning prefix.
    WARNING = (
        "OpenJDK 64-Bit Server VM warning: Options -Xverify:none and -noverify "
        "were deprecated in JDK 13 and will likely be removed in a future release."
    )

    def frame(self, payload, separator):
        return separator.join(
            (
                "Windows fallback: native x86_64-pc-windows-msvc target and SDK; GNU cross compilation is not exercised.",
                "BuildBuddy API key is not available; using local Bazel configuration.",
                "BuildBuddy key unavailable; using local Bazel configuration.",
                "Extracting Bazel installation...",
                self.WARNING,
                "INFO: fixture",
                payload,
            )
        )

    def test_observed_fixed_jvm_warning_frames_provider_record(self):
        record = packages()[0][0]
        for separator in ("\n", "\r\n"):
            with self.subTest(separator=separator):
                text = self.frame(m.SENTINEL + json.dumps(record), separator)
                self.assertEqual(m.parse_provider(text, record["label"]), record)

    def test_observed_fixed_jvm_warning_frames_action_json(self):
        value = graph()
        for separator in ("\n", "\r\n"):
            with self.subTest(separator=separator):
                self.assertEqual(
                    m.parse_json(self.frame(json.dumps(value), separator)), value
                )

    def test_jvm_allowance_rejects_changed_warning_and_fatal_lines(self):
        record = packages()[0][0]
        for line in (
            "prefix " + self.WARNING,
            self.WARNING + " PRIVATE_SECRET",
            " " + self.WARNING,
            self.WARNING + " ",
            self.WARNING.lower(),
            self.WARNING[:-1],
            self.WARNING.replace("JDK 13", "JDK 14"),
            self.WARNING.replace("-noverify", "-PRIVATE_SECRET"),
            self.WARNING.replace("were deprecated", "failed"),
            self.WARNING.replace("warning:", "fatal error:"),
            "FATAL: " + self.WARNING,
            "OpenJDK 64-Bit Server VM warning: Could not create the Java Virtual Machine.",
        ):
            for before in (True, False):
                with self.subTest(line=line, before=before):
                    for payload, provider in (
                        (json.dumps(graph()), False),
                        (m.SENTINEL + json.dumps(record), True),
                    ):
                        text = (
                            line + "\n" + payload if before else payload + "\n" + line
                        )
                        with self.assertRaises(m.RejectedWrapperLine):
                            if provider:
                                m.parse_provider(text, record["label"])
                            else:
                                m.parse_json(text)

    def test_fixed_warning_preserves_duplicate_and_malformed_guards(self):
        record = packages()[0][0]
        for body in ("{}\n{}", '{"a":1,"a":2}', '{"actions": [', "{bad}"):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_json(self.WARNING + "\n" + body)
        provider = m.SENTINEL + json.dumps(record)
        for body in (
            provider + "\n" + provider,
            m.SENTINEL + '{"label":"x","label":"x"}',
            m.SENTINEL + "{",
        ):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_provider(self.WARNING + "\n" + body, record["label"])


class BazeliskDownloadFramingTests(unittest.TestCase):
    # Synthetic timestamps only. The actual LOCALNATIVE observed stamp remains
    # in the separately owned private fixture, never in tracked source.
    STAMPS = ("2004/02/29 23:59:59", "2001/02/03 04:05:06")
    BODY = (
        "Downloading https://releases.bazel.build/9.2.0/release/"
        "bazel-9.2.0-windows-x86_64.exe..."
    )

    def test_known_official_download_frames_provider_record(self):
        record = packages()[0][0]
        payload = m.SENTINEL + json.dumps(record)
        for stamp in self.STAMPS:
            for separator in ("\n", "\r\n"):
                for before in (True, False):
                    with self.subTest(stamp=stamp, separator=separator, before=before):
                        line = stamp + " " + self.BODY
                        text = separator.join(
                            (line, payload) if before else (payload, line)
                        )
                        self.assertEqual(
                            m.parse_provider(text, record["label"]), record
                        )

    def test_known_official_download_frames_action_json(self):
        value = graph()
        payload = json.dumps(value)
        for stamp in self.STAMPS:
            for separator in ("\n", "\r\n"):
                for before in (True, False):
                    with self.subTest(stamp=stamp, separator=separator, before=before):
                        line = stamp + " " + self.BODY
                        text = separator.join(
                            (line, payload) if before else (payload, line)
                        )
                        self.assertEqual(m.parse_json(text), value)

    def test_download_notice_rejects_near_urls_secrets_and_fatal_lines(self):
        record = packages()[0][0]
        exact = self.STAMPS[0] + " " + self.BODY
        for line in (
            "prefix " + exact,
            " " + exact,
            exact + " ",
            exact + " PRIVATE_SECRET",
            exact.lower(),
            exact[:-1],
            exact[:30],
            exact.replace("https://", "http://"),
            exact.replace("releases.bazel.build", "PRIVATE.invalid"),
            exact.replace("https://", "https://user:PRIVATE_SECRET@"),
            exact.replace("9.2.0", "9.2.1"),
            exact.replace(".exe...", ".exe?token=PRIVATE_SECRET..."),
            exact.replace(".exe...", ".exe#PRIVATE_SECRET..."),
            exact.replace(".exe...", ".exe.extra..."),
            exact.replace("Downloading", "FATAL: Downloading"),
            "FATAL: " + exact,
            exact.replace("Downloading", "Download failed for"),
            "x" * 193,
        ):
            for before in (True, False):
                for payload, provider in (
                    (json.dumps(graph()), False),
                    (m.SENTINEL + json.dumps(record), True),
                ):
                    with self.subTest(line=line, before=before, provider=provider):
                        text = (
                            line + "\n" + payload if before else payload + "\n" + line
                        )
                        with self.assertRaises(m.RejectedWrapperLine) as raised:
                            if provider:
                                m.parse_provider(text, record["label"])
                            else:
                                m.parse_json(text)
                        self.assertEqual(
                            raised.exception.args,
                            ("Unexpected wrapper output outside JSON",),
                        )
                        self.assertNotIn("PRIVATE", json.dumps(vars(raised.exception)))

    def test_calendar_invalid_or_non_ascii_timestamps_remain_rejected(self):
        record = packages()[0][0]
        for stamp in (
            "0000/01/01 00:00:00",
            "2001/02/29 04:05:06",
            "2004/13/01 04:05:06",
            "2004/04/31 04:05:06",
            "2004/02/29 24:05:06",
            "2004/02/29 04:60:06",
            "2004/02/29 04:05:60",
            "2004/2/29 04:05:06",
            "2004/02/29\t04:05:06",
            "２００４/02/29 04:05:06",
            "2004/02/29 04:05:06Z",
        ):
            line = stamp + " " + self.BODY
            for before in (True, False):
                for payload, provider in (
                    (json.dumps(graph()), False),
                    (m.SENTINEL + json.dumps(record), True),
                ):
                    with self.subTest(stamp=stamp, before=before, provider=provider):
                        text = (
                            line + "\n" + payload if before else payload + "\n" + line
                        )
                        with self.assertRaises(m.RejectedWrapperLine):
                            if provider:
                                m.parse_provider(text, record["label"])
                            else:
                                m.parse_json(text)

    def test_notice_bound_precedes_matching(self):
        with mock.patch.object(
            m.re, "fullmatch", side_effect=AssertionError("Oversized line inspected")
        ):
            self.assertFalse(m.is_bazelisk_download_notice("x" * 193))

    def test_notice_keeps_duplicate_and_malformed_guards(self):
        line = self.STAMPS[0] + " " + self.BODY + "\n"
        record = packages()[0][0]
        for body in ("{}\n{}", '{"a":1,"a":2}', '{"actions": [', "{bad}"):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_json(line + body)
        provider = m.SENTINEL + json.dumps(record)
        for body in (
            provider + "\n" + provider,
            m.SENTINEL + '{"label":"x","label":"x"}',
            m.SENTINEL + "{",
        ):
            with self.subTest(body=body), self.assertRaises(m.EvidenceError):
                m.parse_provider(line + body, record["label"])


class RejectedLineShapeTests(unittest.TestCase):
    def test_indented_progress_remains_rejected_for_provider_and_action(self):
        record = packages()[0][0]
        line = "    currently loading: PRIVATE_SYNTHETIC_PACKAGE"
        for before in (True, False):
            for provider in (True, False):
                with self.subTest(before=before, provider=provider):
                    payload = (
                        m.SENTINEL + json.dumps(record)
                        if provider
                        else json.dumps(graph())
                    )
                    text = line + "\n" + payload if before else payload + "\n" + line
                    with self.assertRaises(m.RejectedWrapperLine):
                        if provider:
                            m.parse_provider(text, record["label"])
                        else:
                            m.parse_json(text)

    def test_every_shape_keeps_secret_bait_and_message_private(self):
        secret = "PRIVATE_URL_https://private.invalid/C:/private?token=SECRET"
        for prefix, four, whitespace, esc, continuation, control in (
            ("    ", True, False, False, "none", False),
            ("\t", False, True, False, "none", True),
            (" ", False, True, False, "none", False),
            ("\x1b[32m", False, False, True, "none", True),
            ("    Fetching ", True, False, False, "fetch-detail", False),
            ("unknown ", False, False, False, "none", False),
        ):
            with self.subTest(prefix=prefix):
                error = m.RejectedWrapperLine(prefix + secret)
                self.assertEqual(
                    error.args, ("Unexpected wrapper output outside JSON",)
                )
                self.assertEqual(
                    error.shape,
                    {
                        "schema": "rejected-wrapper-line-shape-v2",
                        "starts_with_four_ascii_spaces": four,
                        "other_ascii_leading_whitespace": whitespace,
                        "esc_control_in_prefix": esc,
                        "fixed_bazel_continuation": continuation,
                        "public_emitter_prefix": "none",
                        "leading_unicode_bom": False,
                        "unicode_replacement_in_prefix": False,
                        "non_ascii_in_prefix": False,
                        "ascii_control_in_prefix": control,
                    },
                )
                self.assertEqual(set(vars(error)), {"shape"})
                self.assertNotIn("PRIVATE", json.dumps(vars(error)))
                self.assertNotIn("SECRET", str(error))

    def test_fixed_public_prefixes_export_no_timestamp_or_warning_body(self):
        cases = (
            ("OpenJDK 64-Bit Server VM warning: ", "openjdk-64bit-server-vm-warning"),
            ("2004/02/29 04:05:06 ", "go-standard-datetime-header"),
            ("2012/12/31 23:59:59 ", "go-standard-datetime-header"),
            ("unknown ", "none"),
        )
        headers = []
        for prefix, emitter in cases:
            with self.subTest(emitter=emitter, prefix=prefix):
                error = m.RejectedWrapperLine(
                    prefix + "PRIVATE_https://private.invalid/C:/private?token=SECRET"
                )
                expected = {
                    "schema": "rejected-wrapper-line-shape-v2",
                    "starts_with_four_ascii_spaces": False,
                    "other_ascii_leading_whitespace": False,
                    "esc_control_in_prefix": False,
                    "fixed_bazel_continuation": "none",
                    "public_emitter_prefix": emitter,
                    "leading_unicode_bom": False,
                    "unicode_replacement_in_prefix": False,
                    "non_ascii_in_prefix": False,
                    "ascii_control_in_prefix": False,
                }
                self.assertEqual(error.shape, expected)
                self.assertEqual(set(vars(error)), {"shape"})
                self.assertEqual(
                    error.args, ("Unexpected wrapper output outside JSON",)
                )
                serialized = json.dumps(vars(error))
                for bait in ("PRIVATE", "SECRET", "2004", "2012", "private.invalid"):
                    self.assertNotIn(bait, serialized)
                if emitter == "go-standard-datetime-header":
                    headers.append(error.shape)
        self.assertEqual(headers[0], headers[1])

    def test_public_prefix_near_misses_and_invalid_calendars_are_none(self):
        for prefix in (
            "openjdk 64-Bit Server VM warning: ",
            "OpenJDK 64-Bit Server VM warning:",
            "OpenJDK 64-Bit Server VM warning:PRIVATE",
            "2001/02/29 04:05:06 ",
            "0000/01/01 04:05:06 ",
            "2004/13/01 04:05:06 ",
            "2004/04/31 04:05:06 ",
            "2004/02/29 24:05:06 ",
            "2004/02/29 04:60:06 ",
            "2004/02/29 04:05:60 ",
            "2004/2/29 04:05:06 ",
            "２００４/02/29 04:05:06 ",
            "2004/02/29 04:05:06Z ",
            "2004/02/29 04:05:06\t",
        ):
            with self.subTest(prefix=prefix):
                self.assertEqual(
                    m.RejectedWrapperLine(prefix + "SECRET").shape[
                        "public_emitter_prefix"
                    ],
                    "none",
                )
        # Classification covers the header only; text after its separator is
        # untrusted body, including a timezone-looking word or arbitrary URL.
        self.assertEqual(
            m.RejectedWrapperLine("2004/02/29 04:05:06 Z SECRET").shape[
                "public_emitter_prefix"
            ],
            "go-standard-datetime-header",
        )

    def test_unicode_and_control_observations_are_bounded_properties(self):
        for character, field in (
            ("\ufeff", "non_ascii_in_prefix"),
            ("\ufffd", "unicode_replacement_in_prefix"),
            ("é", "non_ascii_in_prefix"),
            ("\x00", "ascii_control_in_prefix"),
            ("\x7f", "ascii_control_in_prefix"),
            ("\x1b", "esc_control_in_prefix"),
        ):
            for offset in (63, 64):
                with self.subTest(character=character, offset=offset):
                    error = m.RejectedWrapperLine("x" * offset + character + "SECRET")
                    self.assertEqual(error.shape[field], offset == 63)
                    self.assertFalse(error.shape["leading_unicode_bom"])
                    self.assertNotIn("SECRET", json.dumps(vars(error)))
        for body in (
            "OpenJDK 64-Bit Server VM warning: SECRET",
            "2004/02/29 04:05:06 SECRET",
        ):
            error = m.RejectedWrapperLine("\ufeff" + body)
            self.assertTrue(error.shape["leading_unicode_bom"])
            self.assertTrue(error.shape["non_ascii_in_prefix"])
            self.assertEqual(error.shape["public_emitter_prefix"], "none")

    def test_new_shapes_do_not_accept_spoofed_provider_or_action_framing(self):
        record = packages()[0][0]
        for line in (
            "OpenJDK 64-Bit Server VM warning: PRIVATE_SECRET",
            "2004/02/29 04:05:06 PRIVATE_SECRET",
            "\ufeffINFO: PRIVATE_SECRET",
            "\ufffdPRIVATE_SECRET",
        ):
            for before in (True, False):
                for provider in (True, False):
                    with self.subTest(line=line, before=before, provider=provider):
                        payload = (
                            m.SENTINEL + json.dumps(record) if provider else "{}"
                        )
                        text = (
                            line + "\n" + payload if before else payload + "\n" + line
                        )
                        with self.assertRaises(m.RejectedWrapperLine):
                            if provider:
                                m.parse_provider(text, record["label"])
                            else:
                                m.parse_json(text)

    def test_new_diagnostic_fields_come_only_from_first_rejected_line(self):
        record = packages()[0][0]
        for provider in (True, False):
            with self.subTest(provider=provider):
                payload = m.SENTINEL + json.dumps(record) if provider else "{}"
                text = (
                    "OpenJDK 64-Bit Server VM warning: FIRST_SECRET\n"
                    "\ufeff\ufffdSECOND_SECRET\n" + payload
                )
                with self.assertRaises(m.RejectedWrapperLine) as raised:
                    if provider:
                        m.parse_provider(text, record["label"])
                    else:
                        m.parse_json(text)
                self.assertEqual(
                    raised.exception.shape["public_emitter_prefix"],
                    "openjdk-64bit-server-vm-warning",
                )
                self.assertFalse(raised.exception.shape["leading_unicode_bom"])
                self.assertFalse(
                    raised.exception.shape["unicode_replacement_in_prefix"]
                )
                self.assertFalse(raised.exception.shape["non_ascii_in_prefix"])
                self.assertNotIn("SECRET", json.dumps(vars(raised.exception)))

    def test_only_fixed_64_code_point_prefix_is_inspected(self):
        class PrefixOnly(str):
            def __getitem__(self, key):
                self.test.assertEqual(key, slice(None, 64))
                return "x" * 64

        private = PrefixOnly("    Fetching SECRET")
        private.test = self
        error = m.RejectedWrapperLine(private)
        self.assertEqual(error.shape["fixed_bazel_continuation"], "none")
        self.assertFalse(error.shape["starts_with_four_ascii_spaces"])
        self.assertFalse(
            m.RejectedWrapperLine("x" * 64 + "\x1bSECRET").shape[
                "esc_control_in_prefix"
            ]
        )

    def test_spoof_and_absence_do_not_authenticate_or_accept_lines(self):
        for line in (
            "    Fetching SPOOFED_SECRET",
            "    SECRET",
            "\tSECRET",
            "\x1bSECRET",
            "otherSECRET",
        ):
            with self.subTest(line=line), self.assertRaises(m.RejectedWrapperLine):
                m.parse_json(line + "\n{}")
        self.assertEqual(m.parse_json("INFO: accepted SECRET\n{}"), {})
        with self.assertRaises(m.EvidenceError) as raised:
            m.parse_json("not JSON")
        self.assertFalse(hasattr(raised.exception, "shape"))

    def test_first_rejected_surrounding_line_in_provider_and_action_framing(self):
        record = packages()[0][0]
        first, later = "\tFIRST_SECRET", "    Fetching LATER_SECRET"
        for before in (True, False):
            for provider in (True, False):
                payload = m.SENTINEL + json.dumps(record) if provider else "{}"
                rejected = first + "\n" + later
                text = "INFO: ACCEPTED_SECRET\n" + (
                    rejected + "\n" + payload if before else payload + "\n" + rejected
                )
                with (
                    self.subTest(before=before, provider=provider),
                    self.assertRaises(m.RejectedWrapperLine) as raised,
                ):
                    if provider:
                        m.parse_provider(text, record["label"])
                    else:
                        m.parse_json(text)
                self.assertEqual(
                    raised.exception.shape, m.RejectedWrapperLine(first).shape
                )

    def test_full_bounded_capture_produces_one_small_fixed_record(self):
        import time

        start = time.monotonic()
        text = "x" * (m.LIMIT - len("SECRET\n{}")) + "SECRET\n{}"
        self.assertEqual(len(text), m.LIMIT)
        with self.assertRaises(m.RejectedWrapperLine) as raised:
            m.parse_json(text)
        self.assertLess(len(json.dumps(raised.exception.shape)), 1024)
        self.assertNotIn("SECRET", json.dumps(raised.exception.shape))
        self.assertLess(time.monotonic() - start, 10)

    def run_main_rejection(
        self, provider, *, source_changed=False, deadline_expired=False, line=None
    ):
        records = {p["label"]: p for p in packages()[0]}
        captured, statuses, rejected = [], [], []
        secret_line = (
            "    Fetching https://PRIVATE.invalid/?token=SECRET"
            if line is None
            else line
        )

        def capture(command, output, budget, timeout=45, *, env=None):
            captured.append(command)
            if "cquery" in command:
                payload = m.SENTINEL + json.dumps(records[command[-1]])
                reject_here = provider
            else:
                payload = json.dumps(graph())
                reject_here = True
            if reject_here:
                rejected.append(True)
                payload = "INFO: ACCEPTED_SECRET\n" + secret_line + "\n" + payload
            output.write_text(payload, encoding="utf-8")
            return {"oversize": False, "timeout": False, "exit_code": 0}

        def git(*args):
            if args == ("rev-parse", "HEAD"):
                return "head"
            if args == ("rev-parse", "HEAD^{tree}"):
                return "tree"
            statuses.append(True)
            return " M owned" if source_changed and len(statuses) > 1 else ""

        with tempfile.TemporaryDirectory() as directory:
            shell, root = Path(directory) / "bash.exe", Path(directory) / "evidence"
            shell.touch()
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
                mock.patch.dict(os.environ, diagnostic_parent(directory), clear=True),
                mock.patch.object(m, "git", side_effect=git),
                mock.patch.object(m, "capture", side_effect=capture),
                mock.patch.object(
                    m.time,
                    "monotonic",
                    side_effect=lambda: 571 if deadline_expired and rejected else 0,
                ),
                mock.patch("builtins.print") as printed,
                self.assertRaises(m.EvidenceError),
            ):
                m.main()
            printed.assert_not_called()
            self.assertEqual(len(captured), 1 if provider else 8)
            self.assertEqual(
                [p.name for p in (root / "upload").iterdir()], ["manifest.json"]
            )
            raw = (root / "upload" / "manifest.json").read_bytes()
            self.assertNotIn(b"PRIVATE", raw)
            self.assertNotIn(b"SECRET", raw)
            manifest = json.loads(raw)
            self.assertEqual(
                manifest["rejected_wrapper_line_shape"],
                m.RejectedWrapperLine(secret_line).shape,
            )
            self.assertNotIn("artifacts", manifest)
            self.assertEqual(manifest["source_unchanged"], not source_changed)
            return manifest

    def test_real_main_propagates_provider_and_action_shape_without_artifacts(self):
        for provider in (True, False):
            with self.subTest(provider=provider):
                manifest = self.run_main_rejection(provider)
                self.assertEqual(manifest["status"], "failed")
                self.assertEqual(
                    manifest["failure"], "Unexpected wrapper output outside JSON"
                )

    def test_real_main_propagates_new_fixed_fields_without_private_values(self):
        for prefix, emitter, bom, replacement in (
            (
                "OpenJDK 64-Bit Server VM warning: ",
                "openjdk-64bit-server-vm-warning",
                False,
                False,
            ),
            ("2004/02/29 04:05:06 ", "go-standard-datetime-header", False, False),
            ("\ufeff", "none", True, False),
            ("\ufffd", "none", False, True),
        ):
            for provider in (True, False):
                with self.subTest(provider=provider, emitter=emitter, bom=bom):
                    manifest = self.run_main_rejection(
                        provider, line=prefix + "PRIVATE_URL?token=SECRET"
                    )
                    shape = manifest["rejected_wrapper_line_shape"]
                    self.assertEqual(shape["schema"], "rejected-wrapper-line-shape-v2")
                    self.assertEqual(shape["public_emitter_prefix"], emitter)
                    self.assertEqual(shape["leading_unicode_bom"], bom)
                    self.assertEqual(
                        shape["unicode_replacement_in_prefix"], replacement
                    )
                    self.assertNotIn("2004", json.dumps(manifest))

    def test_shape_does_not_bypass_source_or_final_deadline_guards(self):
        self.assertEqual(
            self.run_main_rejection(True, source_changed=True)["status"],
            "failed-source-changed",
        )
        manifest = self.run_main_rejection(True, deadline_expired=True)
        self.assertEqual(manifest["status"], "failed-deadline")
        self.assertEqual(
            manifest["failure"], "Configured metadata invocation deadline exhausted"
        )


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
                self.creation_flags = args[5]
                self.creation_cwd = args[7]
                self.child_environment_block = (
                    None
                    if args[6] is None
                    else c.string_at(args[6], self.environment_size)
                )
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


class ProgressObservationTests(unittest.TestCase):
    def test_pinned_plain_progress_prefixes_cr_lf_and_private_suffixes(self):
        raw = (
            b"Extracting Bazel installation...\r\n"
            b"Computing main repo mapping: PRIVATE_URL\r"
            b"Loading: PRIVATE_PATH\nAnalyzing: PRIVATE_LABEL\r\n"
            b"    Fetching PRIVATE_TOKEN\nLoading: other\n"
        )
        result = m.classify_progress(raw)
        self.assertEqual(
            result["prefix_line_counts"],
            dict(zip(m.PROGRESS_MARKERS, (1, 1, 2, 1, 1))),
        )
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertEqual(m.classify_failure(raw)["progress_observations"], result)

    def test_exact_spoof_is_observation_not_authenticated_execution(self):
        result = m.classify_progress(b"Analyzing: arbitrary user-controlled text")
        self.assertEqual(result["prefix_line_counts"]["analysis"], 1)
        self.assertEqual(set(result), {"schema", "count_limit", "prefix_line_counts"})

    def test_unrecognized_wrappers_substrings_and_split_markers_stay_unknown(self):
        unknown = m.classify_progress(b"")
        for raw in (
            b"help: Loading: PRIVATE",
            b"XAnalyzing: PRIVATE",
            b" Loading: PRIVATE",
            b"Loading:PRIVATE",
            b"Loading:\n PRIVATE",
            b"Computing main repo mapping:\n PRIVATE",
            b"Analyzing\r: PRIVATE",
            b"Extracting Bazel installation... PRIVATE",
            b"Extracting Bazel installation....",
            b"    FetchingPRIVATE",
            b"\x1b[32mLoading: PRIVATE",
            b"[12:30:00] Loading: PRIVATE",
            b"loading: PRIVATE",
            b"INFO: Starting Bazel in batch mode.",
        ):
            with self.subTest(raw=raw):
                self.assertEqual(m.classify_progress(raw), unknown)

    def test_cap_and_count_saturation_preserve_closed_vocabulary(self):
        raw = b"Loading: SECRET\n" * (m.PROGRESS_COUNT_LIMIT + 2)
        result = m.classify_progress(raw)
        self.assertEqual(
            result["prefix_line_counts"]["loading"], m.PROGRESS_COUNT_LIMIT
        )
        self.assertEqual(set(result["prefix_line_counts"]), set(m.PROGRESS_MARKERS))
        self.assertTrue(
            all(type(v) is int for v in result["prefix_line_counts"].values())
        )
        self.assertNotIn("SECRET", json.dumps(result))
        self.assertEqual(
            m.classify_progress(b"x" * m.LIMIT + b"\nLoading: SECRET"),
            m.classify_progress(b""),
        )

    def test_full_capture_adversarial_shapes_finish_within_capture_budget(self):
        import time

        start = time.monotonic()
        unknown = m.classify_progress(b"")
        for unit in (b"x", b"x\n", b"x\r\n", b"LoadingX: ", b"AnalyzingX: "):
            raw = (unit * (m.LIMIT // len(unit) + 1))[: m.LIMIT]
            self.assertEqual(m.classify_progress(raw), unknown)
        raw = (b"Loading: PRIVATE\r\n" * (m.LIMIT // 18 + 1))[: m.LIMIT]
        self.assertEqual(
            m.classify_progress(raw)["prefix_line_counts"]["loading"],
            m.PROGRESS_COUNT_LIMIT,
        )
        self.assertLess(time.monotonic() - start, 15)


class QueryBudgetTests(unittest.TestCase):
    def test_full_window_and_cleanup_at_exact_admission_boundary(self):
        for timeout in (120, 45, 90):
            with (
                self.subTest(timeout=timeout),
                mock.patch.object(m.time, "monotonic", return_value=100),
            ):
                m.admit_query(100 + timeout + 15, timeout)
                with self.assertRaisesRegex(
                    m.EvidenceError, "^Insufficient global budget"
                ):
                    m.admit_query(100 + timeout + 15 - 0.001, timeout)

    def test_same_deadline_never_extends_or_shortens_later_phase(self):
        with mock.patch.object(m.time, "monotonic", side_effect=(0, 120, 451)):
            m.admit_query(510, 120)
            m.admit_query(510, 45)
            with self.assertRaisesRegex(m.EvidenceError, "^Insufficient global budget"):
                m.admit_query(510, 45)
        self.assertEqual(m.INVOCATION_SECONDS, 570)
        self.assertEqual(m.POSTPROCESSING_RESERVE_SECONDS, 60)
        self.assertEqual(m.OWNED_CLEANUP_RESERVE_SECONDS, 15)

    def run_deadline_failure(self, clock, expected, expected_captures):
        records = {p["label"]: p for p in packages()[0]}

        def capture(command, output, budget, timeout=45, *, env=None):
            if "cquery" in command:
                output.write_text(m.SENTINEL + json.dumps(records[command[-1]]))
            else:
                output.write_text(json.dumps(graph()))
            return {"oversize": False, "timeout": False, "exit_code": 0}

        with tempfile.TemporaryDirectory() as directory:
            shell, root = Path(directory) / "bash.exe", Path(directory) / "evidence"
            shell.touch()
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
                mock.patch.dict(os.environ, diagnostic_parent(directory), clear=True),
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
                mock.patch.object(m.time, "monotonic", side_effect=clock),
                mock.patch.object(m, "capture", side_effect=capture) as captured,
                mock.patch.object(m, "package_proof", return_value=[]),
                self.assertRaisesRegex(m.EvidenceError, expected),
            ):
                m.main()
            self.assertEqual(captured.call_count, expected_captures)
            self.assertEqual(
                [p.name for p in (root / "upload").iterdir()], ["manifest.json"]
            )
            return json.loads((root / "upload" / "manifest.json").read_bytes())

    def test_first_window_refused_before_any_launch_and_no_partial_acceptance(self):
        manifest = self.run_deadline_failure(
            (0, 375.001, 375.002), "^Insufficient global budget", 0
        )
        self.assertEqual(manifest["status"], "failed")
        self.assertEqual(manifest["phases"], [])
        self.assertTrue(manifest["source_unchanged"])

    def test_later_window_refused_without_retry_or_partial_acceptance(self):
        manifest = self.run_deadline_failure(
            (0, 0, 120, 451, 451), "^Insufficient global budget", 2
        )
        self.assertEqual(manifest["status"], "failed")
        self.assertEqual(len(manifest["phases"]), 2)

    def test_expired_postprocessing_never_publishes_success(self):
        manifest = self.run_deadline_failure(
            (0,) * 9 + (571, 571), "^Configured metadata invocation deadline", 8
        )
        self.assertEqual(manifest["status"], "failed-deadline")
        self.assertEqual(len(manifest["phases"]), 8)
        self.assertTrue(manifest["source_unchanged"])


class EnvironmentTests(unittest.TestCase):
    def test_encoding_sorting_astral_values_and_empty_block(self):
        env = {"z": "last", "ProgramFiles(x86)": "C:/tools", "A": "astral-\U0001f642"}
        expected = "A=astral-\U0001f642\0ProgramFiles(x86)=C:/tools\0z=last\0\0".encode(
            "utf-16-le"
        )
        self.assertEqual(owned.environment_block(env), expected)
        self.assertEqual(owned.environment_block({}), b"\0\0\0\0")
        self.assertEqual(env["A"], "astral-\U0001f642")

    def test_invalid_environment_fails_before_owned_resource_creation(self):
        for env in (
            {"bad=key": "PRIVATE"},
            {"Path": "a", "PATH": "b"},
            {"A": "PRIVATE\0value"},
            {"A": "\ud800"},
            {"\u00e9": "PRIVATE"},
            {"A": 3},
            {3: "PRIVATE"},
        ):
            api = FakeAPI()
            with (
                self.subTest(env=repr(env)),
                self.assertRaisesRegex(
                    ValueError, "^Invalid explicit child environment$"
                ),
            ):
                owned.capture_owned(
                    ["qualified.exe"], Path("unused"), 1, 1, api=api, env=env
                )
            self.assertEqual(api.events, [])

    def test_required_preservation_policy_secrets_identity_and_accounting(self):
        parent = diagnostic_parent("C:/temp")
        parent.update(
            {
                "https_proxy": "https://PRIVATE-proxy",
                "SSL_CERT_FILE": "C:/PRIVATE-cert",
                "ProgramFiles(x86)": "C:/PRIVATE-tools",
                "PYTHONPATH": "C:/\U0001f642",
                "UNRELATED_SECRET": "PRIVATE-irrelevant",
                "BUILDBUDDY_API_KEY": "PRIVATE-key",
                "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR": "PRIVATE-log",
                "BAZEL_OUTPUT_USER_ROOT": "ordinary-root",
            }
        )
        original = parent.copy()
        child, metrics = m.diagnostic_environment(
            parent, Path("unique-root"), "head", "tree"
        )
        self.assertEqual(parent, original)
        for name, value in parent.items():
            if name.upper() in m.REQUIRED_CHILD_ENV | m.OPTIONAL_CHILD_ENV:
                self.assertEqual(
                    child[m.CHILD_ENV_SPELLINGS.get(name.upper(), name)].encode(
                        "utf-16-le"
                    ),
                    value.encode("utf-16-le"),
                )
        self.assertEqual(child["BAZEL_OUTPUT_USER_ROOT"], "unique-root")
        self.assertEqual(child["VOICE_CONFIGURED_METADATA_MODE"], "bounded-v1")
        self.assertTrue(metrics["metadata_named_path_bindings_requested"])
        self.assertEqual(
            metrics["selected_path_utf16_units"],
            m.utf16_units(parent["CODEX_BAZEL_WINDOWS_PATH"]),
        )
        self.assertEqual(
            metrics["repeated_path_utf16_units_removed_projection"],
            3 * (1 + m.utf16_units(parent["CODEX_BAZEL_WINDOWS_PATH"])),
        )
        for name in (
            "UNRELATED_SECRET",
            "BUILDBUDDY_API_KEY",
            "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR",
        ):
            self.assertNotIn(name, child)
        self.assertEqual(metrics["child"]["entry_count"], len(child))
        self.assertEqual(
            metrics["child"]["block_utf16_units"],
            len(owned.environment_block(child)) // 2,
        )
        self.assertEqual(
            metrics["child_block_utf16_units"], metrics["child"]["block_utf16_units"]
        )
        self.assertEqual(
            metrics["child"]["unescaped_client_env_utf16_units"],
            sum(
                len(("--client_env=" + k + "=" + v + " ").encode("utf-16-le")) // 2
                for k, v in child.items()
            ),
        )
        self.assertNotIn("PRIVATE", json.dumps(metrics))
        self.assertNotIn("https://", json.dumps(metrics))
        # A large configured PATH is preserved. Projection is not a final Bazel
        # argv budget gate; rc/JVM and Git Bash transformations remain unknown.
        parent["PATH"] = "p" * 40000
        child, metrics = m.diagnostic_environment(parent, Path("root"), "head", "tree")
        self.assertEqual(child["PATH"], parent["PATH"])
        self.assertGreater(metrics["child"]["unescaped_client_env_utf16_units"], 32768)
        self.assertEqual(
            metrics["client_env_accounting"],
            "unescaped-projection-not-final-command-length",
        )

    def test_windows_normalized_names_keep_bash_sdk_and_proxy_consumers(self):
        parent = diagnostic_parent("temp")
        parent["HTTPS_PROXY"] = "PRIVATE-proxy"
        child, metrics = m.diagnostic_environment(parent, Path("root"), "head", "tree")
        for spelling in m.SDK_ENV_NAMES:
            self.assertEqual(child[spelling], parent[spelling.upper()])
        self.assertEqual(child["https_proxy"], parent["HTTPS_PROXY"])
        self.assertNotIn("HTTPS_PROXY", child)
        self.assertTrue(metrics["included_values_unchanged"])

    def test_every_required_input_missing_and_identity_mismatch_fail_closed(self):
        for name in m.REQUIRED_CHILD_ENV:
            for value in (None, ""):
                parent = diagnostic_parent("temp")
                if value is None:
                    del parent[name]
                else:
                    parent[name] = value
                with (
                    self.subTest(name=name, value=value),
                    self.assertRaisesRegex(
                        m.EvidenceError, "Missing required diagnostic environment"
                    ),
                ):
                    m.diagnostic_environment(parent, Path("root"), "head", "tree")
        for name in (
            "VOICE_DIAGNOSTIC_HEAD",
            "VOICE_DIAGNOSTIC_TREE",
            "GITHUB_SHA",
            "VOICE_WINDOWS_SYSTEM_ROOT",
            "VOICE_WINDOWS_HOST_ARCH",
            "GITHUB_REPOSITORY",
            "GITHUB_EVENT_NAME",
            "GITHUB_JOB",
        ):
            parent = diagnostic_parent("temp")
            parent[name] = "PRIVATE-mismatch"
            with self.assertRaises(m.EvidenceError):
                m.diagnostic_environment(parent, Path("root"), "head", "tree")
        parent = diagnostic_parent("temp")
        parent["Path"] = "PRIVATE-conflict"
        with self.assertRaisesRegex(
            m.EvidenceError, "Conflicting diagnostic environment aliases"
        ):
            m.diagnostic_environment(parent, Path("root"), "head", "tree")

    def test_mock_win32_receives_explicit_bytes_unicode_flag_and_same_cwd(self):
        env = {"SystemRoot": "C:/Windows", "PRIVATE_FIXTURE": "\U0001f642"}
        expected = owned.environment_block(env)
        api = FakeAPI()
        api.environment_size = len(expected)
        parent = dict(os.environ)
        with tempfile.TemporaryDirectory() as directory:
            owned.capture_owned(
                ["qualified.exe"], Path(directory) / "raw", 64, 60, api=api, env=env
            )
        self.assertEqual(api.child_environment_block, expected)
        self.assertTrue(api.creation_flags & 0x400)
        self.assertIsNone(api.creation_cwd)
        self.assertEqual(dict(os.environ), parent)
        self.assertLess(
            api.events.index("AssignProcessToJobObject"),
            api.events.index("ResumeThread"),
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

        def capture(command, output, budget, timeout=45, *, env=None):
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
                    diagnostic_parent(directory),
                    clear=True,
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
                original_parent = dict(os.environ)
                with self.assertRaisesRegex(m.EvidenceError, "Provider capture failed"):
                    m.main()
                self.assertEqual(dict(os.environ), original_parent)
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

    def test_main_missing_required_environment_stops_before_any_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            shell = Path(directory) / "bash.exe"
            shell.touch()
            root = Path(directory) / "evidence"
            parent = diagnostic_parent(directory)
            del parent["INCLUDE"]
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
                mock.patch.dict(os.environ, parent, clear=True),
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
                mock.patch.object(m, "capture") as captured,
            ):
                before = dict(os.environ)
                with self.assertRaisesRegex(
                    m.EvidenceError, "^Missing required diagnostic environment$"
                ):
                    m.main()
                self.assertEqual(dict(os.environ), before)
            captured.assert_not_called()
            manifest = json.loads((root / "upload" / "manifest.json").read_bytes())
            self.assertEqual(manifest["phases"], [])
            self.assertEqual(
                manifest["failure"], "Missing required diagnostic environment"
            )
            self.assertTrue(manifest["source_unchanged"])
            self.assertEqual(list((root / "private").iterdir()), [])

    def test_main_emits_batch_queries_with_absolute_shell_and_unique_private_root(self):
        records = {p["label"]: p for p in packages()[0]}
        seen = []

        def capture(command, output, budget, timeout=45, *, env=None):
            seen.append((command, timeout, env["BAZEL_OUTPUT_USER_ROOT"]))
            self.assertEqual(
                os.environ["BAZEL_OUTPUT_USER_ROOT"], "ordinary-runtime-root"
            )
            self.assertNotIn("BUILDBUDDY_API_KEY", env)
            self.assertNotIn("CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR", env)
            self.assertEqual(env["VOICE_CONFIGURED_METADATA_MODE"], "bounded-v1")
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
                            **diagnostic_parent(directory),
                            "BAZEL_OUTPUT_USER_ROOT": "ordinary-runtime-root",
                            "BUILDBUDDY_API_KEY": "PRIVATE-credential",
                            "CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR": "ordinary-log-dir",
                        },
                        clear=True,
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
                    original_parent = dict(os.environ)
                    m.main()
                    self.assertEqual(dict(os.environ), original_parent)
            self.assertEqual(len(seen), 16)
            self.assertNotEqual(seen[0][2], seen[8][2])
            for index, (command, timeout, root) in enumerate(seen):
                self.assertEqual(command[0], str(shell))
                query = "cquery" if "cquery" in command else "aquery"
                self.assertEqual(command[command.index(query) - 1], "--batch")
                target_separator = len(command) - 2
                self.assertEqual(command[target_separator], "--")
                self.assertEqual(command.count("--noshow_progress"), 1)
                self.assertLess(command.index("--noshow_progress"), target_separator)
                self.assertEqual(
                    [arg for arg in command if "show_progress" in arg],
                    ["--noshow_progress"],
                )
                self.assertEqual(
                    timeout, 120 if index % 8 == 0 else 45 if query == "cquery" else 90
                )
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
        self.assertIsNone(api.child_environment_block)
        self.assertFalse(api.creation_flags & 0x400)

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
