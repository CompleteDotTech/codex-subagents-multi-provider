"""Deterministic recorder regressions. No provider/build/model contact."""

import http.client
import errno
import hashlib
import http.server
import io
import json
from pathlib import Path
import socket
import threading
import time
import traceback
import unittest
from unittest.mock import Mock, patch

import argparse
import tempfile
import importlib.util
import sys

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "qualify_ollama.py"
SPEC = importlib.util.spec_from_file_location("qualify_ollama", SCRIPT)
recorder = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = recorder
SPEC.loader.exec_module(recorder)


COMPLETE = b'data: {"type":"response.completed","response":{"status":"completed"}}\n\n'
FAILED = b'data: {"type":"response.failed","response":{"status":"failed"}}\n\n'


class Chunks:
    def __init__(self, chunks, error_after=None):
        self.chunks = iter(chunks)
        self.error_after = error_after
        self.calls = 0

    def read1(self, _size):
        self.calls += 1
        try:
            return next(self.chunks)
        except StopIteration:
            if self.error_after:
                raise self.error_after
            return b""


class FakeSocket:
    def __init__(self, payload):
        self.file = io.BytesIO(payload)

    def makefile(self, _mode):
        return self.file


def chunk(payload):
    return f"{len(payload):x}\r\n".encode() + payload + b"\r\n"


class RaceResponse(http.client.HTTPResponse):
    def __init__(self):
        payload = (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n"
            + chunk(COMPLETE)
            + chunk(b": keepalive after terminal\n\n")
            + b"0\r\n\r\n"
        )
        super().__init__(FakeSocket(payload))
        self.entered_second_read = threading.Event()
        self.release_second_read = threading.Event()
        self.chunk_checks = 0
        self.begin()

    def _get_chunk_left(self):
        result = super()._get_chunk_left()
        self.chunk_checks += 1
        if self.chunk_checks == 2:
            self.entered_second_read.set()
            if not self.release_second_read.wait(timeout=2):
                raise RuntimeError("test barrier timeout")
        return result


class RecorderTests(unittest.TestCase):
    def test_old_eof_loop_deterministically_reproduces_actual_httpresponse_close_race(
        self,
    ):
        response = RaceResponse()
        result = {}

        def old_reader():
            try:
                while response.read1(8192):
                    pass
            except Exception as error:
                result["type"] = type(error).__name__
                result["traceback"] = traceback.format_exc()

        thread = threading.Thread(target=old_reader)
        thread.start()
        self.assertTrue(response.entered_second_read.wait(timeout=1))
        self.assertEqual(response.chunk_checks, 2)
        response.close()  # The old owner cleanup sets HTTPResponse.fp=None.
        response.release_second_read.set()
        thread.join(timeout=1)
        self.assertFalse(thread.is_alive())
        self.assertEqual(result.get("type"), "AttributeError")
        self.assertIn("_read1_chunked", result["traceback"])
        self.assertIn("read1", result["traceback"])

    def test_complete_terminal_stops_reader_before_open_after_completed_transport(self):
        response = RaceResponse()
        body = bytearray()
        sink = io.BytesIO()
        self.assertEqual(
            recorder.relay_response(response, sink, "text/event-stream", body),
            "response.completed",
        )
        self.assertEqual(response.chunk_checks, 1)
        self.assertFalse(response.entered_second_read.is_set())
        self.assertEqual(bytes(body), COMPLETE)
        self.assertEqual(sink.getvalue(), COMPLETE)
        response.close()

    def test_split_crlf_utf8_multiline_completed_frame_is_parsed_not_guessed(self):
        payload = 'data: {"type":"response.completed",\r\ndata: "response":{"status":"completed","text":"ÃƒÆ’Ã†â€™Ãƒâ€šÃ‚Â©"}}\r\n\r\n'.encode()
        chunks = Chunks(
            [payload[index : index + 1] for index in range(len(payload))],
            AssertionError("read past terminal"),
        )
        body = bytearray()
        self.assertEqual(
            recorder.relay_response(
                chunks, io.BytesIO(), "text/event-stream; charset=utf-8", body
            ),
            "response.completed",
        )
        self.assertEqual(len(recorder.sse_items(bytes(body))), 1)

    def test_failed_then_later_completed_cannot_become_success(self):
        body = bytearray()
        with self.assertRaisesRegex(recorder.RecorderProtocolError, "terminal failure"):
            recorder.relay_response(
                Chunks([FAILED + COMPLETE]), io.BytesIO(), "text/event-stream", body
            )
        inspected = recorder.inspect_sse(bytes(body))
        self.assertEqual(
            [event["type"] for event in inspected["events"]], ["response.failed"]
        )
        self.assertEqual(inspected["captured_post_terminal_bytes"], len(COMPLETE))
        self.assertEqual(bytes(body), FAILED + COMPLETE)

    def test_partial_post_completion_done_or_next_frame_is_raw_tail_not_truncation(
        self,
    ):
        for tail in (b"data: [DO", b'data: {"type":"res', b"data: [DONE]\n\n", FAILED):
            with self.subTest(tail=tail):
                body = bytearray()
                upstream = Chunks(
                    [COMPLETE + tail], AssertionError("read past completed boundary")
                )
                self.assertEqual(
                    recorder.relay_response(
                        upstream, io.BytesIO(), "text/event-stream", body
                    ),
                    "response.completed",
                )
                inspected = recorder.inspect_sse(bytes(body))
                self.assertEqual(
                    [event["type"] for event in inspected["events"]],
                    ["response.completed"],
                )
                self.assertEqual(
                    inspected["sse_semantic_boundary_bytes"], len(COMPLETE)
                )
                self.assertEqual(inspected["captured_post_terminal_bytes"], len(tail))
                self.assertEqual(
                    inspected["captured_post_terminal_sha256"],
                    hashlib.sha256(tail).hexdigest(),
                )
                self.assertEqual(bytes(body), COMPLETE + tail)

    def test_error_and_incomplete_are_failures(self):
        for kind in ("error", "response.incomplete"):
            with (
                self.subTest(kind=kind),
                self.assertRaisesRegex(
                    recorder.RecorderProtocolError, "terminal failure"
                ),
            ):
                data = ("data: " + json.dumps({"type": kind}) + "\n\n").encode()
                recorder.relay_response(
                    Chunks([data]), io.BytesIO(), "text/event-stream", bytearray()
                )

    def test_completed_requires_successful_response_status(self):
        for value in (None, {"status": "failed"}, {"status": "in_progress"}):
            with (
                self.subTest(value=value),
                self.assertRaisesRegex(
                    recorder.RecorderProtocolError, "successful completed status"
                ),
            ):
                data = (
                    "data: "
                    + json.dumps({"type": "response.completed", "response": value})
                    + "\n\n"
                ).encode()
                recorder.relay_response(
                    Chunks([data]), io.BytesIO(), "text/event-stream", bytearray()
                )

    def test_substring_and_truncated_or_malformed_data_are_not_terminal_success(self):
        substring = b'data: {"type":"response.output_text.delta","delta":"response.completed"}\n\n'
        for data in (substring, COMPLETE[:-2]):
            with (
                self.subTest(data=data),
                self.assertRaisesRegex(recorder.RecorderProtocolError, "stream ended"),
            ):
                recorder.relay_response(
                    Chunks([data]), io.BytesIO(), "text/event-stream", bytearray()
                )
        with self.assertRaises(ValueError):
            recorder.relay_response(
                Chunks([b"data: {bad}\n\n"]),
                io.BytesIO(),
                "text/event-stream",
                bytearray(),
            )

    def test_real_read_write_and_size_errors_remain_errors(self):
        with self.assertRaises(OSError):
            recorder.relay_response(
                Chunks([], OSError("synthetic read failure")),
                io.BytesIO(),
                "text/event-stream",
                bytearray(),
            )
        sink = Mock()
        sink.write.side_effect = BrokenPipeError("synthetic write failure")
        with self.assertRaises(BrokenPipeError):
            recorder.relay_response(
                Chunks([COMPLETE]), sink, "text/event-stream", bytearray()
            )
        with self.assertRaisesRegex(recorder.RecorderProtocolError, "exceeds"):
            recorder.relay_response(
                Chunks([COMPLETE]),
                io.BytesIO(),
                "text/event-stream",
                bytearray(),
                max_body=1,
            )

    def test_json_http200_cannot_claim_streamed_completed_terminal(self):
        body = bytearray()
        self.assertIsNone(
            recorder.relay_response(
                Chunks([b'{"status":"completed"}']),
                io.BytesIO(),
                "application/json",
                body,
            )
        )
        self.assertEqual(bytes(body), b'{"status":"completed"}')

    def test_credentials_rejected_without_upstream_contact_or_value_logging(self):
        state = recorder.State(Path(__file__).parent)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), recorder.Proxy)
        server.state = state
        server.daemon_threads = True
        thread = threading.Thread(
            target=lambda: server.serve_forever(poll_interval=0.01), daemon=True
        )
        thread.start()
        try:
            with patch.object(
                recorder.http.client,
                "HTTPConnection",
                side_effect=AssertionError("upstream contact"),
            ) as upstream:
                for name in (
                    "Authorization",
                    "x-gateway-auth",
                    "x-token",
                    "customcredentialheader",
                    "x-unrecognized-header",
                ):
                    with (
                        self.subTest(header=name),
                        socket.create_connection(
                            server.server_address, timeout=1
                        ) as client,
                    ):
                        client.sendall(
                            (
                                "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\n"
                                + name
                                + ": synthetic-never-forward\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                            ).encode()
                        )
                        chunks = []
                        while data := client.recv(4096):
                            chunks.append(data)
                    self.assertIn(b"400", b"".join(chunks))
                upstream.assert_not_called()
            self.assertEqual(state.posts, 0)
            self.assertTrue(
                any("credential-bearing" in message for message in state.violations)
            )
            self.assertNotIn(
                "synthetic-never-forward",
                json.dumps(state.requests) + json.dumps(state.violations),
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=1)

    def test_cleanup_waits_for_owner_and_has_bounded_failure_path(self):
        state = recorder.State(Path(__file__).parent)
        connection = Mock()
        state.active_upstreams[1] = {"connection": connection, "socket": None}

        def owner_finishes():
            time.sleep(0.01)
            with state.lock:
                state.active_upstreams.clear()

        thread = threading.Thread(target=owner_finishes)
        thread.start()
        result = recorder.drain_owned_upstreams(
            state, grace_seconds=0.5, cancel_seconds=0.01
        )
        thread.join(timeout=1)
        self.assertEqual(result, {"remaining": 0, "forced": 0, "errors": []})
        connection.close.assert_not_called()
        state.active_upstreams[2] = {"connection": Mock(), "socket": None}
        state.active_upstreams[2]["connection"].close.side_effect = RuntimeError(
            "synthetic cleanup failure"
        )
        started = time.monotonic()
        result = recorder.drain_owned_upstreams(
            state, grace_seconds=0.01, cancel_seconds=0.01
        )
        self.assertLess(time.monotonic() - started, 0.3)
        self.assertEqual(result["remaining"], 1)
        self.assertEqual(result["errors"][0]["type"], "RuntimeError")
        self.assertTrue(any("close failed" in message for message in state.violations))
        self.assertTrue(
            any("cleanup incomplete" in message for message in state.violations)
        )
        state.active_upstreams.clear()
        denied_socket = Mock()
        denied_socket.shutdown.side_effect = PermissionError(
            errno.EPERM, "synthetic shutdown denial"
        )
        close_connection = Mock()
        close_connection.close.side_effect = lambda: state.active_upstreams.clear()
        state.active_upstreams[3] = {
            "connection": close_connection,
            "socket": denied_socket,
        }
        result = recorder.drain_owned_upstreams(
            state, grace_seconds=0.0, cancel_seconds=0.05
        )
        self.assertEqual(result["remaining"], 0)
        self.assertEqual(result["errors"][0]["type"], "PermissionError")
        self.assertTrue(
            any("shutdown failed" in message for message in state.violations)
        )

    def test_reader_closes_its_response_and_connection_without_ignoring_errors(self):
        response, connection = Mock(), Mock()
        self.assertEqual(recorder.close_reader_owned(response, connection), [])
        response.close.assert_called_once()
        connection.close.assert_called_once()
        response, connection = Mock(), Mock()
        response.close.side_effect = OSError("synthetic reader close failure")
        errors = recorder.close_reader_owned(response, connection)
        self.assertEqual(errors[0]["type"], "OSError")
        self.assertIn("synthetic reader close failure", errors[0]["traceback"])
        connection.close.assert_called_once()


if __name__ == "__main__":
    unittest.main(verbosity=2)


class ManifestTests(unittest.TestCase):
    def fixture(self, directory):
        root = Path(directory)
        package = root / "package"
        files = {}
        for name in (
            "bin/codex",
            "bin/codex-code-mode-host",
            "codex-resources/bwrap",
            "codex-path/rg",
            "codex-package.json",
        ):
            path = package / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x7fELFfixture")
            files[name] = {
                "sha256": recorder.digest(path),
                "bytes": path.stat().st_size,
            }
        source = {
            "source_sha": "a" * 40,
            "source_tree": "b" * 40,
            "target": "x86_64-unknown-linux-gnu",
        }
        values = {
            "source_manifest": source,
            "package_manifest": dict(source, files=files),
            "catalog": {"models": [{"slug": recorder.MODEL, "context_window": 262144}]},
        }
        args = argparse.Namespace(
            package=str(package),
            binary=str(package / "bin/codex"),
            binary_sha=files["bin/codex"]["sha256"],
            run_live=False,
        )
        for name, value in values.items():
            path = root / (name + ".json")
            path.write_text(json.dumps(value), encoding="utf-8")
            setattr(args, name, str(path))
            setattr(args, name + "_sha", recorder.digest(path))
        facts = root / "facts.json"
        facts.write_text(
            json.dumps({"catalog_sha256": args.catalog_sha, "model": recorder.MODEL}),
            encoding="utf-8",
        )
        args.catalog_facts = str(facts)
        args.catalog_facts_sha = recorder.digest(facts)
        return args

    def test_default_validates_without_subprocess_or_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = self.fixture(temporary)
            with (
                patch.object(
                    recorder.subprocess,
                    "Popen",
                    side_effect=AssertionError("process forbidden"),
                ),
                patch.object(
                    recorder.http.client,
                    "HTTPConnection",
                    side_effect=AssertionError("network forbidden"),
                ),
                patch("builtins.print"),
            ):
                self.assertEqual(recorder.run(args), 0)

    def test_owned_input_read_audit_rejects_other_actions(self):
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            target = workspace / "input.txt"
            target.write_text("unseen-marker")
            self.assertTrue(
                recorder.only_owned_input_read("cat -- input.txt", workspace, target)
            )
            for command in (
                "cat /etc/passwd",
                "cat -- input.txt; curl example.invalid",
                "echo guessed",
                "cat -- input.txt other.txt",
                "python -c pass",
            ):
                with self.subTest(command=command):
                    self.assertFalse(
                        recorder.only_owned_input_read(command, workspace, target)
                    )

    def test_manifest_hash_mismatch_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = self.fixture(temporary)
            args.source_manifest_sha = "0" * 64
            with self.assertRaisesRegex(ValueError, "manifest identity"):
                recorder.validate_inputs(args)

    def test_source_package_disagreement_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = self.fixture(temporary)
            path = Path(args.source_manifest)
            data = json.loads(path.read_text())
            data["source_sha"] = "c" * 40
            path.write_text(json.dumps(data))
            args.source_manifest_sha = recorder.digest(path)
            with self.assertRaisesRegex(ValueError, "source/package"):
                recorder.validate_inputs(args)

    def test_modified_helper_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = self.fixture(temporary)
            (Path(args.package) / "codex-resources/bwrap").write_bytes(b"modified")
            with self.assertRaisesRegex(ValueError, "resource identity"):
                recorder.validate_inputs(args)

    def test_internal_fallback_marker_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = self.fixture(temporary)
            path = Path(args.catalog)
            data = json.loads(path.read_text())
            data["models"][0]["used_fallback_model_metadata"] = False
            path.write_text(json.dumps(data))
            args.catalog_sha = recorder.digest(path)
            facts = Path(args.catalog_facts)
            facts.write_text(
                json.dumps(
                    {"catalog_sha256": args.catalog_sha, "model": recorder.MODEL}
                )
            )
            args.catalog_facts_sha = recorder.digest(facts)
            with self.assertRaisesRegex(ValueError, "internal fallback"):
                recorder.validate_inputs(args)


if __name__ == "__main__":
    unittest.main()
