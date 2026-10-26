#!/usr/bin/env python3
"""Explicit manual Linux/WSL Ollama qualification. Default: validate inputs only."""

import argparse
import errno
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import signal
import shlex
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import traceback
import urllib.parse
import uuid


MODEL = "gemma4:31b-cloud"
MAX_BODY = 4 * 1024 * 1024
SENSITIVE = ("auth", "cookie", "key", "token", "secret", "credential")
SAFE_HEADERS = frozenset(
    {
        "host",
        "content-length",
        "content-type",
        "accept",
        "accept-encoding",
        "connection",
        "user-agent",
        "x-codex-beta-features",
        "x-codex-window-id",
        "x-codex-turn-metadata",
        "x-client-request-id",
        "session-id",
        "thread-id",
        "originator",
    }
)


def digest(path):
    with open(path, "rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


class RecorderProtocolError(RuntimeError):
    pass


class SseFrames:
    """Inspect complete SSE frames, including fragmented UTF-8/multiline data."""

    def __init__(self, validate_terminal=False):
        self.pending = bytearray()
        self.events = []
        self.validate_terminal = validate_terminal
        self.completed = False
        self.terminal_kind = None
        self.consumed_bytes = 0

    def feed(self, block):
        self.pending.extend(block)
        if self.terminal_kind is not None:
            return self.completed
        while boundary := re.search(rb"\r\n\r\n|\r\n\n|\n\r\n|\n\n", self.pending):
            frame = bytes(self.pending[: boundary.start()])
            del self.pending[: boundary.end()]
            self.consumed_bytes += boundary.end()
            data = []
            for line in frame.splitlines():
                if line.startswith(b"data:"):
                    value = line[5:]
                    data.append(value[1:] if value.startswith(b" ") else value)
            if not data:
                continue
            value = b"\n".join(data).decode("utf-8", errors="strict")
            if value.strip() == "[DONE]":
                if self.validate_terminal and not self.completed:
                    raise RecorderProtocolError(
                        "DONE before a successful completed response"
                    )
                self.terminal_kind = "done_without_completed"
                return False
            event = json.loads(value)
            if not isinstance(event, dict):
                raise RecorderProtocolError("SSE data is not an event object")
            self.events.append(event)
            kind = event.get("type")
            if kind in ("error", "response.failed", "response.incomplete"):
                self.terminal_kind = kind
                if self.validate_terminal:
                    raise RecorderProtocolError("terminal failure event: " + kind)
                return False
            if kind == "response.completed":
                response = event.get("response")
                if (
                    not isinstance(response, dict)
                    or response.get("status") != "completed"
                ):
                    self.terminal_kind = "completed_with_invalid_status"
                    if self.validate_terminal:
                        raise RecorderProtocolError(
                            "completed event has no successful completed status"
                        )
                    return False
                self.completed = True
                self.terminal_kind = kind
                return True
        return self.completed


def inspect_sse(payload):
    frames = SseFrames()
    frames.feed(payload)
    if frames.pending.strip() and frames.terminal_kind is None:
        raise RecorderProtocolError("unterminated SSE frame")
    return {
        "events": frames.events,
        "semantic_terminal_type": frames.terminal_kind,
        "sse_semantic_boundary_bytes": frames.consumed_bytes,
        "captured_post_terminal_bytes": len(frames.pending),
        "captured_post_terminal_sha256": hashlib.sha256(frames.pending).hexdigest()
        if frames.pending
        else None,
    }


def sse_items(payload):
    return inspect_sse(payload)["events"]


def relay_response(upstream, sink, content_type, response_body, max_body=MAX_BODY):
    frames = (
        SseFrames(validate_terminal=True)
        if content_type.startswith("text/event-stream")
        else None
    )
    while block := upstream.read1(8192):
        response_body.extend(block)
        if len(response_body) > max_body:
            raise RecorderProtocolError("response exceeds probe bound")
        sink.write(block)
        sink.flush()
        if frames is not None and frames.feed(block):
            # response.completed is the client protocol's terminal event. The
            # reader closes its own response instead of waiting for transport
            # EOF while another thread closes HTTPResponse.fp underneath read1.
            return "response.completed"
    if frames is not None:
        raise RecorderProtocolError(
            "stream ended before a successful completed response"
        )
    return None


def drain_owned_upstreams(state, grace_seconds=2.0, cancel_seconds=5.0):
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline:
        with state.lock:
            if not state.active_upstreams:
                return {"remaining": 0, "forced": 0, "errors": []}
        time.sleep(0.01)
    with state.lock:
        upstreams = list(state.active_upstreams.values())
    errors = []
    for upstream in upstreams:
        if upstream["socket"] is not None:
            try:
                upstream["socket"].shutdown(socket.SHUT_RDWR)
            except OSError as error:
                if error.errno not in (errno.EBADF, errno.ENOTCONN):
                    errors.append(
                        {
                            "type": type(error).__name__,
                            "traceback": traceback.format_exc(),
                        }
                    )
                    state.violation("owned upstream shutdown failed")
        try:
            upstream["connection"].close()
        except Exception as error:
            errors.append(
                {"type": type(error).__name__, "traceback": traceback.format_exc()}
            )
            state.violation("owned upstream close failed")
    deadline = time.monotonic() + cancel_seconds
    while time.monotonic() < deadline:
        with state.lock:
            if not state.active_upstreams:
                return {"remaining": 0, "forced": len(upstreams), "errors": errors}
        time.sleep(0.01)
    with state.lock:
        remaining = len(state.active_upstreams)
    if remaining:
        state.violation("owned upstream cleanup incomplete")
    return {"remaining": remaining, "forced": len(upstreams), "errors": errors}


def close_reader_owned(upstream, connection):
    errors = []
    for owned in (upstream, connection):
        if owned is None:
            continue
        try:
            owned.close()
        except Exception as error:
            errors.append(
                {"type": type(error).__name__, "traceback": traceback.format_exc()}
            )
    return errors


class State:
    def __init__(self, directory):
        self.directory = directory
        self.lock = threading.Lock()
        self.requests = []
        self.violations = []
        self.posts = 0
        self.active_upstreams = {}

    def violation(self, message):
        with self.lock:
            self.violations.append(message)


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def reject(self, reason, status=400):
        self.server.state.violation(reason)
        payload = json.dumps(
            {"error": {"message": reason, "type": "probe_guard"}}
        ).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        state = self.server.state
        parsed = urllib.parse.urlsplit(self.path)
        if (
            parsed.scheme
            or parsed.netloc
            or parsed.query
            or parsed.path not in ("/v1/responses", "/v1/models")
        ):
            return self.reject("path outside fixed probe upstream")
        names = [name.lower() for name in self.headers]
        self.connection.settimeout(10)
        if any(any(marker in name for marker in SENSITIVE) for name in names):
            return self.reject("credential-bearing header rejected before forwarding")
        if set(names) - SAFE_HEADERS:
            return self.reject("unexpected header rejected before forwarding")
        if self.command == "POST" and parsed.path != "/v1/responses":
            return self.reject("unexpected POST path")
        if self.command == "GET" and parsed.path != "/v1/models":
            return self.reject("unexpected GET path")
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            return self.reject("invalid request length")
        if not 0 <= length <= MAX_BODY:
            return self.reject("request exceeds probe bound")
        body = self.rfile.read(length)
        if len(body) != length:
            return self.reject("truncated request")
        document = None
        if self.command == "POST":
            try:
                document = json.loads(body)
            except ValueError:
                return self.reject("invalid request JSON")
            if not isinstance(document, dict):
                return self.reject("request JSON must be an object")
            if document.get("model") != MODEL or document.get("stream") is not True:
                return self.reject("model/stream differs from qualified client route")
            if any(
                key in document
                for key in ("previous_response_id", "conversation", "prompt_cache_key")
            ):
                return self.reject("stateful continuation in stateless request")
        with state.lock:
            if self.command == "POST":
                if state.posts >= 4:
                    return self.reject_unlocked("forwarded POST budget exhausted")
                state.posts += 1
            identifier = len(state.requests)
            record = {
                "id": identifier,
                "method": self.command,
                "path": self.path,
                "header_names": names,
                "credential_headers_present": False,
                "request": document,
                "upstream": "http://127.0.0.1:11434",
            }
            state.requests.append(record)
        connection = http.client.HTTPConnection("127.0.0.1", 11434, timeout=45)
        with state.lock:
            state.active_upstreams[identifier] = {
                "connection": connection,
                "socket": None,
            }
        response_body = bytearray()
        upstream = None
        try:
            headers = {
                name: value
                for name, value in self.headers.items()
                if name.lower() in ("content-type", "accept")
            }
            headers["Accept-Encoding"] = "identity"
            connection.request(
                self.command, self.path, body=body if length else None, headers=headers
            )
            with state.lock:
                state.active_upstreams[identifier]["socket"] = connection.sock
            upstream = connection.getresponse()
            record["status"] = upstream.status
            record["content_type"] = upstream.getheader("Content-Type", "")
            self.send_response(upstream.status)
            for key in ("Content-Type", "Retry-After"):
                if upstream.getheader(key):
                    self.send_header(key, upstream.getheader(key))
            self.send_header("Connection", "close")
            self.end_headers()
            record["relay_terminal"] = relay_response(
                upstream, self.wfile, record["content_type"], response_body
            )
        except Exception as error:
            record["transport_error"] = type(error).__name__
            record["transport_traceback"] = traceback.format_exc()
            state.violation("proxy transport or response bound failure")
        finally:
            record["reader_cleanup_errors"] = close_reader_owned(upstream, connection)
            if record["reader_cleanup_errors"]:
                state.violation("proxy reader cleanup failed")
            self.close_connection = True
            path = state.directory / f"response-{identifier}.body"
            path.write_bytes(response_body)
            record["response_file"] = path.name
            record["response_sha256"] = digest(path)
            if record.get("content_type", "").startswith("text/event-stream"):
                try:
                    record.update(inspect_sse(response_body))
                except (UnicodeError, ValueError, RecorderProtocolError):
                    state.violation("invalid SSE/JSON response")
            with state.lock:
                state.active_upstreams.pop(identifier, None)

    def reject_unlocked(self, reason):
        # Caller holds the state lock, so avoid re-entering State.violation.
        self.server.state.violations.append(reason)
        payload = json.dumps(
            {"error": {"message": reason, "type": "probe_guard"}}
        ).encode()
        self.send_response(429)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True


def response_calls(events):
    calls = []
    for event in events:
        item = event.get("item", {})
        if (
            event.get("type") == "response.output_item.done"
            and item.get("type") == "function_call"
        ):
            calls.append(item)
        if event.get("type") == "response.completed":
            calls.extend(
                item
                for item in event.get("response", {}).get("output", [])
                if item.get("type") == "function_call"
            )
    return list(
        {(call.get("call_id"), call.get("name")): call for call in calls}.values()
    )


def only_owned_input_read(command, workspace, input_file):
    """Audit observed native command text; this does not intercept execution."""
    try:
        tokens = shlex.split(command)
        if tokens and Path(tokens[0]).name in ("bash", "sh"):
            if len(tokens) < 3 or tokens[-2] not in ("-c", "-lc"):
                return False
            if any(
                option not in ("--noprofile", "--norc", "-l") for option in tokens[1:-2]
            ):
                return False
            tokens = shlex.split(tokens[-1])
        if not tokens or tokens[0] not in ("cat", "/bin/cat", "/usr/bin/cat"):
            return False
        arguments = tokens[1:]
        if arguments and arguments[0] == "--":
            arguments = arguments[1:]
        if len(arguments) != 1:
            return False
        selected = Path(arguments[0])
        if not selected.is_absolute():
            selected = workspace / selected
        return selected.resolve(strict=True) == input_file.resolve(strict=True)
    except (OSError, ValueError):
        return False


def run(args):
    identity = validate_inputs(args)
    if not args.run_live:
        print(json.dumps({"live": False, "validated": identity}, indent=2))
        return 0
    if os.name != "posix" or not Path("/proc/self/exe").exists():
        raise RuntimeError("live qualification requires Linux/WSL with procfs")
    binary = Path(args.binary).resolve(strict=True)
    approved_catalog = Path(args.catalog).resolve(strict=True)
    artifact = Path(args.artifact_root).resolve(strict=True)
    owned = Path(tempfile.mkdtemp(prefix="issue22-native-", dir="/tmp"))
    home = owned / "home"
    workspace = owned / "workspace"
    home.mkdir()
    workspace.mkdir()
    catalog = home / "catalog.json"
    shutil.copyfile(approved_catalog, catalog)
    marker = str(uuid.uuid4())
    input_file = workspace / "input.txt"
    input_file.write_text(marker, encoding="utf-8")
    expected = "PROBE:" + marker
    final = owned / "final.txt"
    output_dir = artifact / ("live-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()))
    output_dir.mkdir()
    for label in ("catalog", "catalog_facts", "source_manifest", "package_manifest"):
        shutil.copyfile(getattr(args, label), output_dir / (label + ".json"))
    state = State(output_dir)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    server.daemon_threads = True
    server.state = state
    proxy_thread = threading.Thread(target=server.serve_forever, daemon=True)
    proxy_thread.start()
    environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(home),
        "CODEX_HOME": str(home),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "TERM": "dumb",
        "CODEX_OSS_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
    }
    prompt = (
        "Run exactly `cat -- input.txt` using an available local shell/exec tool. Return exactly PROBE: followed by its file content. "
        "Do not guess the content. Do not modify files, use network tools, or launch subagents."
    )
    command = [
        str(binary),
        "exec",
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--cd",
        str(workspace),
        "--model",
        MODEL,
        "-c",
        'model_provider="ollama"',
        "-c",
        'web_search="disabled"',
        "-c",
        "features.multi_agent=false",
        "-c",
        "model_context_window=262144",
        "-c",
        "model_catalog_json=" + json.dumps(str(catalog)),
        "--output-last-message",
        str(final),
        prompt,
    ]
    report = {
        "source_sha": identity["source_sha"],
        "source_tree": identity["source_tree"],
        "binary": str(binary),
        "binary_sha256": args.binary_sha,
        "catalog_sha256": args.catalog_sha,
        "catalog_path": str(catalog),
        "catalog_source": str(approved_catalog),
        "catalog_previously_native_typed_validated": False,
        "verified_manifests": identity,
        "validator_sha256": digest(Path(__file__)),
        "model": MODEL,
        "upstream": "http://127.0.0.1:11434/v1",
        "cloud_backed_model": True,
        "configured_client_context_window": 262144,
        "server_maximum_context_stress_tested": False,
        "owned_directory": str(owned),
        "prompt_contains_uuid": marker in prompt,
        "command": command,
        "environment_names": sorted(environment),
        "maximum_active_seconds": 150,
        "maximum_teardown_seconds": 30,
        "maximum_forwarded_posts": 4,
        "proactive_command_blocking": False,
        "scope_enforcement": "native read-only sandbox plus post-run qualification audit",
        "delivery_or_mixed_provider_acceptance": False,
    }
    started = time.monotonic()
    process = None
    try:
        with (
            open(output_dir / "cli.jsonl", "wb") as stdout,
            open(output_dir / "cli.stderr", "wb") as stderr,
        ):
            process = subprocess.Popen(
                command,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
                start_new_session=True,
            )
            report["pid"] = process.pid
            for _ in range(40):
                proc = Path(f"/proc/{process.pid}/exe")
                try:
                    resolved = proc.resolve(strict=True)
                    if resolved == binary:
                        report["proc_exe"] = str(resolved)
                        report["proc_exe_sha256"] = digest(proc)
                        break
                except OSError:
                    pass
                if process.poll() is not None:
                    break
                time.sleep(0.05)
            try:
                report["exit"] = process.wait(
                    timeout=max(1, 150 - (time.monotonic() - started))
                )
            except subprocess.TimeoutExpired:
                report["timeout"] = True
                os.killpg(process.pid, signal.SIGINT)
                try:
                    report["exit"] = process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    report["exit"] = process.wait(timeout=5)
    except Exception as error:
        report["launcher_error"] = type(error).__name__
        report["launcher_traceback"] = traceback.format_exc()
    finally:
        if process is not None and process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            report["exit"] = process.wait(timeout=5)
        server.shutdown()
        report["upstream_cleanup"] = drain_owned_upstreams(state)
        server.server_close()
        proxy_thread.join(timeout=3)
        report["owned_upstreams_remaining"] = report["upstream_cleanup"]["remaining"]
        report["seconds"] = round(time.monotonic() - started, 3)
    posts = [item for item in state.requests if item["method"] == "POST"]
    report["wire_reasoning"] = [item["request"].get("reasoning") for item in posts]
    report["wire_models"] = [item["request"].get("model") for item in posts]
    report["wire_stream"] = [item["request"].get("stream") for item in posts]
    report["wire_parallel_tool_calls"] = [
        item["request"].get("parallel_tool_calls") for item in posts
    ]
    report["canonical_response_models"] = sorted(
        {
            event.get("response", {}).get("model")
            for item in posts
            for event in item.get("events", [])
            if event.get("type") in ("response.created", "response.completed")
        }
    )
    report["known_model_alias_mapping"] = {
        "local_request_tag": "gemma4:31b-cloud",
        "service_parent_model": "gemma4:31b",
    }
    report["post_count"] = len(posts)
    report["violations"] = state.violations
    report["initial_request_contains_uuid"] = bool(
        posts and marker in json.dumps(posts[0]["request"])
    )
    calls = [
        (index, call)
        for index, item in enumerate(posts)
        for call in response_calls(item.get("events", []))
    ]
    allowed_wire_names = {
        "exec",
        "exec_command",
        "shell",
        "shell_command",
        "functions__exec",
        "functions__exec_command",
        "functions__shell",
        "functions__shell_command",
    }
    report["off_scope_wire_calls"] = [
        call.get("name")
        for _, call in calls
        if call.get("name") not in allowed_wire_names
    ]
    if report["off_scope_wire_calls"]:
        state.violation("wire tool call outside read-only probe tool scope")
    paired = []
    for index, call in calls:
        call_id = call.get("call_id")
        if not call_id:
            continue
        for following in posts[index + 1 :]:
            inputs = following["request"].get("input", [])
            if not isinstance(inputs, list):
                continue
            matching_call = any(
                item.get("type") == "function_call" and item.get("call_id") == call_id
                for item in inputs
            )
            matching_output = any(
                item.get("type") == "function_call_output"
                and item.get("call_id") == call_id
                and marker in str(item.get("output", ""))
                for item in inputs
            )
            if matching_call and matching_output:
                paired.append(
                    {
                        "call_id": call_id,
                        "name": call.get("name"),
                        "arguments": call.get("arguments"),
                    }
                )
    report["consumed_function_pairs"] = paired
    events = []
    for line in (output_dir / "cli.jsonl").read_text(errors="replace").splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            state.violation("invalid native CLI JSONL")
    all_items = [
        item.get("item", {})
        for item in events
        if item.get("type") in ("item.started", "item.updated", "item.completed")
    ]
    report["off_scope_item_types"] = sorted(
        {
            item.get("type", "missing")
            for item in all_items
            if item.get("type")
            not in ("command_execution", "agent_message", "reasoning")
        }
    )
    report["off_scope_commands"] = [
        item.get("command")
        for item in all_items
        if item.get("type") == "command_execution"
        and not only_owned_input_read(item.get("command", ""), workspace, input_file)
    ]
    if report["off_scope_item_types"] or report["off_scope_commands"]:
        state.violation("native tool/command evidence outside owned input read")
    commands = [
        item.get("item", {}) for item in events if item.get("type") == "item.completed"
    ]
    report["native_successful_command_with_uuid"] = any(
        item.get("type") == "command_execution"
        and item.get("status") == "completed"
        and item.get("exit_code") == 0
        and marker in item.get("aggregated_output", "")
        for item in commands
    )
    report["final_text"] = final.read_text().strip() if final.exists() else None
    report["input_unchanged"] = input_file.read_text() == marker
    report["catalog_unchanged"] = digest(catalog) == args.catalog_sha
    report["fallback_warnings"] = [
        item.get("item", {}).get("message")
        for item in events
        if item.get("item", {}).get("type") == "error"
        and "fallback" in item.get("item", {}).get("message", "").lower()
    ]
    report["native_error_events"] = [
        event.get("type")
        for event in events
        if event.get("type") in ("error", "turn.failed")
    ]
    report["stderr_fallback"] = (
        "fallback" in (output_dir / "cli.stderr").read_text(errors="replace").lower()
    )
    terminal_errors = [
        event.get("type")
        for item in posts
        for event in item.get("events", [])
        if event.get("type") in ("error", "response.failed", "response.incomplete")
    ]
    report["terminal_errors"] = terminal_errors
    report["completed_responses"] = sum(
        event.get("type") == "response.completed"
        for item in posts
        for event in item.get("events", [])
    )
    report["function_schema_advertised"] = bool(
        posts
        and any(
            tool.get("type") == "function"
            for tool in posts[0]["request"].get("tools", [])
        )
    )
    report["success"] = bool(
        report.get("exit") == 0
        and not report.get("launcher_error")
        and not report.get("timeout")
        and not state.violations
        and report.get("proc_exe_sha256") == args.binary_sha
        and not report["initial_request_contains_uuid"]
        and len(posts) >= 2
        and all(item.get("status") == 200 for item in posts)
        and paired
        and all(item.get("relay_terminal") == "response.completed" for item in posts)
        and report["native_successful_command_with_uuid"]
        and report["final_text"] == expected
        and report["owned_upstreams_remaining"] == 0
        and not report["upstream_cleanup"]["errors"]
        and not proxy_thread.is_alive()
        and report["seconds"] <= 180
        and report["input_unchanged"]
        and not terminal_errors
        and report["completed_responses"] >= 2
        and report["function_schema_advertised"]
        and report["catalog_unchanged"]
        and not report["fallback_warnings"]
        and not report["stderr_fallback"]
        and not report["native_error_events"]
        and bool(report["canonical_response_models"])
        and all(
            model in ("gemma4:31b-cloud", "gemma4:31b")
            for model in report["canonical_response_models"]
        )
    )
    (output_dir / "wire.json").write_text(json.dumps(state.requests, indent=2))
    report["artifact_hashes"] = {
        path.name: digest(path) for path in output_dir.iterdir() if path.is_file()
    }
    (output_dir / "result.json").write_text(json.dumps(report, indent=2))
    print(
        json.dumps(
            {
                "result": str(output_dir / "result.json"),
                "success": report["success"],
                "exit": report.get("exit"),
                "post_count": len(posts),
                "consumed_function_pairs": len(paired),
                "violations": state.violations,
            },
            indent=2,
        )
    )
    return 0 if report["success"] else 1


def checked_json(path, expected):
    path = Path(path).resolve(strict=True)
    if not re.fullmatch(r"[0-9a-f]{64}", expected) or digest(path) != expected:
        raise ValueError("manifest identity mismatch")
    return json.loads(path.read_text(encoding="utf-8"))


def validate_inputs(args):
    """Hash agreement binds operator-reviewed evidence; it is not build attestation."""
    source = checked_json(args.source_manifest, args.source_manifest_sha)
    package = checked_json(args.package_manifest, args.package_manifest_sha)
    facts = checked_json(args.catalog_facts, args.catalog_facts_sha)
    for key in ("source_sha", "source_tree", "target"):
        if source.get(key) != package.get(key) or not source.get(key):
            raise ValueError("source/package identity mismatch: " + key)
    if not re.fullmatch(r"[0-9a-f]{40}", source["source_sha"]) or not re.fullmatch(
        r"[0-9a-f]{40}", source["source_tree"]
    ):
        raise ValueError("invalid Git identity")
    if source["target"] != "x86_64-unknown-linux-gnu":
        raise ValueError("only the qualified native Linux target is supported")
    root = Path(args.package).resolve(strict=True)
    required = {
        "bin/codex",
        "bin/codex-code-mode-host",
        "codex-resources/bwrap",
        "codex-path/rg",
        "codex-package.json",
    }
    if set(package.get("files", {})) != required:
        raise ValueError("incomplete package resource manifest")
    for name, entry in package["files"].items():
        selected = (root / name).resolve(strict=True)
        if not selected.is_relative_to(root) or not selected.is_file():
            raise ValueError("package resource escapes owned package")
        if (
            selected.stat().st_size != entry["bytes"]
            or digest(selected) != entry["sha256"]
        ):
            raise ValueError("package resource identity mismatch: " + name)
    binary = Path(args.binary).resolve(strict=True)
    if binary != (root / "bin/codex").resolve() or digest(binary) != args.binary_sha:
        raise ValueError("binary differs from verified package")
    with binary.open("rb") as handle:
        if handle.read(4) != b"\x7fELF":
            raise ValueError("native ELF required")
    catalog = checked_json(args.catalog, args.catalog_sha)
    if facts.get("catalog_sha256") != args.catalog_sha or facts.get("model") != MODEL:
        raise ValueError("catalog facts identity mismatch")
    models = catalog.get("models", [])
    if (
        len(models) != 1
        or models[0].get("slug") != MODEL
        or models[0].get("context_window") != 262144
    ):
        raise ValueError("catalog differs from bounded qualification policy")
    if "used_fallback_model_metadata" in models[0]:
        raise ValueError("internal fallback marker cannot be supplied by a catalog")
    return {key: source[key] for key in ("source_sha", "source_tree", "target")} | {
        "package_manifest_sha256": args.package_manifest_sha,
        "source_manifest_sha256": args.source_manifest_sha,
        "catalog_facts_sha256": args.catalog_facts_sha,
    }


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--binary-sha", required=True)
    parser.add_argument(
        "--run-live",
        action="store_true",
        help="Explicitly permit up to four model POSTs",
    )
    parser.add_argument("--package", required=True)
    parser.add_argument("--package-manifest", required=True)
    parser.add_argument("--package-manifest-sha", required=True)
    parser.add_argument("--source-manifest", required=True)
    parser.add_argument("--source-manifest-sha", required=True)
    parser.add_argument("--catalog-facts", required=True)
    parser.add_argument("--catalog-facts-sha", required=True)
    parser.add_argument("--artifact-root", required=True)
    parser.add_argument("--catalog", required=True)
    parser.add_argument("--catalog-sha", required=True)
    return run(parser.parse_args(argv))


if __name__ == "__main__":
    raise SystemExit(main())
