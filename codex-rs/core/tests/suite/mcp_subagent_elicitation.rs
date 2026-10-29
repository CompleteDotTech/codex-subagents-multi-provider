//! MCP server elicitations preserve approval policy and signal abandonment before shutdown.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::approvals::ElicitationAction;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use futures::FutureExt as _;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::matchers::body_partial_json;

const SERVER: &str = r#"
import time
CUSTODY_STARTED = time.monotonic()
import json
import sys
import os
import stat
import hashlib

def custody_ready():
    nonce = os.environ.get("CODEX_TEST_MCP_CUSTODY_NONCE")
    directory = os.environ.get("CODEX_TEST_MCP_CUSTODY_DIR")
    if nonce is None and directory is None:
        return
    deadline = CUSTODY_STARTED + 10.0
    def clock():
        if time.monotonic() >= deadline:
            raise RuntimeError("MCP custody original startup deadline")
    def hex64(value):
        return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)
    if sys.platform != "linux" or not hex64(nonce) or directory is None:
        raise RuntimeError("MCP custody requires complete local Linux opt-in")
    temp = os.environ.get("TMPDIR")
    case = os.environ.get("CODEX_TEST_MCP_CUSTODY_CASE_SHA256")
    owner = os.environ.get("CODEX_TEST_MCP_CUSTODY_OWNER_PID", "")
    if temp is None or not hex64(case) or not owner.isascii() or not owner.isdecimal() or str(int(owner)) != owner or int(owner) <= 0:
        raise RuntimeError("MCP custody missing phase/case/owner")
    for name in (temp, directory):
        if len(os.fsencode(name)) > 4096:
            raise RuntimeError("MCP custody directory path bound")
        if not os.path.isabs(name) or os.path.realpath(name) != name:
            raise RuntimeError("MCP custody path is not canonical")
        cursor = name
        while True:
            if stat.S_ISLNK(os.lstat(cursor).st_mode):
                raise RuntimeError("MCP custody symlink directory")
            parent = os.path.dirname(cursor)
            if parent == cursor:
                break
            cursor = parent
    if directory == temp or os.path.commonpath((temp, directory)) != temp:
        raise RuntimeError("MCP custody directory outside phase TMPDIR")
    def private(fd):
        info = os.fstat(fd)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise RuntimeError("MCP custody requires owned0700 directory")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    root = os.open(temp, flags)
    try:
        private(root)
        for component in os.path.relpath(directory, temp).split(os.sep):
            if component in ("", ".", ".."):
                raise RuntimeError("MCP custody directory component")
            child = os.open(component, flags, dir_fd=root)
            os.close(root)
            root = child
            private(root)
        def small(path):
            with open(path, "rb") as stream:
                value = stream.read(4097)
            if len(value) > 4096:
                raise RuntimeError("MCP custody proc bound")
            return value
        pid = os.getpid()
        def birth():
            text = small("/proc/self/stat").decode("utf-8")
            if text.split("(", 1)[0].strip() != str(pid):
                raise RuntimeError("MCP custody PID mismatch")
            fields = text.rsplit(")", 1)[1].split()
            if len(fields) < 20 or fields[0] in ("Z", "X", "x"):
                raise RuntimeError("MCP custody process not live")
            value = (int(fields[19]), int(fields[1]), os.stat("/proc/self").st_uid)
            if value[0] <= 0 or value[1] <= 0 or value[2] != os.geteuid():
                raise RuntimeError("MCP custody birth/owner mismatch")
            return value
        def identity(info):
            return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
        before = birth()
        clock()
        exe = os.open("/proc/self/exe", os.O_RDONLY | os.O_CLOEXEC)
        try:
            info = os.fstat(exe)
            original = identity(info)
            if not stat.S_ISREG(info.st_mode) or info.st_size > 1073741824:
                raise RuntimeError("MCP custody executable bound")
            digest = hashlib.sha256()
            count = 0
            while True:
                clock()
                block = os.read(exe, 65536)
                if not block:
                    break
                count += len(block)
                if count > 1073741824:
                    raise RuntimeError("MCP custody executable read bound")
                digest.update(block)
            exe_hash = digest.hexdigest()
            if identity(os.fstat(exe)) != original or identity(os.stat("/proc/self/exe")) != original or birth() != before:
                raise RuntimeError("MCP custody identity changed while hashing")
            boot = hashlib.sha256(small("/proc/sys/kernel/random/boot_id").strip()).hexdigest()
            receipt = {"schema": 1, "nonce": nonce, "kind": "python", "role": "initialize",
                       "case_sha256": case, "owner_pid": int(owner), "pid": pid,
                       "starttime": before[0], "ppid": before[1], "uid": before[2], "boot_sha256": boot,
                       "exe": {"dev": info.st_dev, "ino": info.st_ino, "size": info.st_size,
                               "mtime_sec": info.st_mtime_ns // 1000000000, "mtime_nsec": info.st_mtime_ns % 1000000000,
                               "ctime_sec": info.st_ctime_ns // 1000000000, "ctime_nsec": info.st_ctime_ns % 1000000000,
                               "sha256": exe_hash, "path_sha256": hashlib.sha256(os.fsencode(os.readlink("/proc/self/exe"))).hexdigest()}}
            payload = json.dumps(receipt, sort_keys=True, separators=(",", ":")).encode("utf-8")
            if len(payload) > 4096:
                raise RuntimeError("MCP custody receipt bound")
            stem = nonce + "-python-" + str(pid) + "-" + str(before[0])
            clock()
            record = os.open(stem + ".json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=root)
            try:
                state = os.fstat(record)
                if not stat.S_ISREG(state.st_mode) or state.st_uid != os.geteuid() or state.st_nlink != 1 or stat.S_IMODE(state.st_mode) != 0o600:
                    raise RuntimeError("MCP custody receipt identity")
                if os.write(record, payload) != len(payload):
                    raise RuntimeError("MCP custody incomplete receipt write")
                os.fsync(record)
            finally:
                os.close(record)
            os.fsync(root)
            def unique(pairs):
                result = {}
                for key, value in pairs:
                    if key in result:
                        raise ValueError("duplicate MCP custody ACK field")
                    result[key] = value
                return result
            while True:
                clock()
                try:
                    ack = os.open(stem + ".ack.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=root)
                except FileNotFoundError:
                    time.sleep(0.005)
                    continue
                try:
                    state = os.fstat(ack)
                    if not stat.S_ISREG(state.st_mode) or state.st_uid != os.geteuid() or state.st_nlink != 1 or stat.S_IMODE(state.st_mode) != 0o600 or state.st_size > 4096:
                        raise RuntimeError("MCP custody ACK identity")
                    value = os.read(ack, 4097)
                    if len(value) > 4096 or identity(os.fstat(ack)) != identity(state):
                        raise RuntimeError("MCP custody ACK changed")
                    value = json.loads(value.decode("utf-8"), object_pairs_hook=unique)
                    expected = {"schema": 1, "nonce": nonce, "pid": pid, "starttime": before[0], "exe_sha256": exe_hash, "case_sha256": case}
                    if value != expected or any(type(value.get(key)) is not int for key in ("schema", "pid", "starttime")):
                        raise RuntimeError("MCP custody ACK mismatch")
                finally:
                    os.close(ack)
                if birth() != before or identity(os.stat("/proc/self/exe")) != original or hashlib.sha256(small("/proc/sys/kernel/random/boot_id").strip()).hexdigest() != boot:
                    raise RuntimeError("MCP custody process changed before ACK admission")
                clock()
                break
        finally:
            os.close(exe)
    finally:
        os.close(root)

def send(message):
    print(json.dumps({"jsonrpc": "2.0", **message}), flush=True)

elicitation = json.loads(sys.argv[1])
pending = None
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        try:
            custody_ready()
        except Exception:
            raise RuntimeError("MCP custody initialization UNQUALIFIED") from None
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "elicitation-test", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "request_input", "inputSchema": {"type": "object", "properties": {}},
                             "annotations": {"readOnlyHint": True}}]}
    elif method == "tools/call":
        pending = request["id"]
        send({"id": "input-request", "method": "elicitation/create", "params": elicitation})
        continue
    elif method is None and request.get("id") == "input-request":
        send({"id": pending, "result": {"content": [{"type": "text", "text": json.dumps(
            request.get("result", request.get("error")))}]}})
        continue
    elif method == "resources/list":
        result = {"resources": []}
    elif method == "resources/templates/list":
        result = {"resourceTemplates": []}
    elif "id" not in request:
        continue
    else:
        result = {}
    send({"id": request["id"], "result": result})
"#;

fn custody_environment() -> Result<Option<std::collections::HashMap<String, String>>> {
    let nonce = std::env::var_os("CODEX_TEST_MCP_CUSTODY_NONCE");
    let directory = std::env::var_os("CODEX_TEST_MCP_CUSTODY_DIR");
    if nonce.is_none() && directory.is_none() {
        return Ok(None);
    }
    anyhow::ensure!(
        nonce.is_some() && directory.is_some(),
        "partial MCP custody opt-in"
    );
    anyhow::ensure!(
        cfg!(target_os = "linux") && !core_test_support::is_remote_test_environment(),
        "MCP custody opt-in requires local Linux, never remote/Wine/Windows"
    );
    let nonce = nonce
        .unwrap()
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid custody nonce"))?;
    anyhow::ensure!(
        nonce.len() == 64
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid custody nonce"
    );
    let directory = directory
        .unwrap()
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid custody directory"))?;
    use sha2::Digest;
    let case = format!(
        "{:x}",
        sha2::Sha256::digest(
            std::thread::current()
                .name()
                .ok_or_else(|| anyhow::anyhow!("actual libtest thread name required"))?
                .as_bytes()
        )
    );
    Ok(Some(std::collections::HashMap::from([
        ("CODEX_TEST_MCP_CUSTODY_NONCE".into(), nonce),
        ("CODEX_TEST_MCP_CUSTODY_DIR".into(), directory),
        ("TMPDIR".into(), std::env::var("TMPDIR")?),
        ("CODEX_TEST_MCP_CUSTODY_CASE_SHA256".into(), case),
        (
            "CODEX_TEST_MCP_CUSTODY_OWNER_PID".into(),
            std::process::id().to_string(),
        ),
    ])))
}

#[derive(Clone, Copy)]
pub(super) enum Caller {
    Root,
    Subagent,
    FullAccessSubagent,
}

#[derive(Clone, Copy)]
pub(super) enum RequestKind {
    BrowserAuth,
    FormInput,
    ShutdownFormInput,
    Permission,
    StrictReview,
}

#[test_case(Caller::FullAccessSubagent, RequestKind::BrowserAuth; "full_access_subagent_preserves_automatic_approval")]
#[test_case(Caller::Root, RequestKind::BrowserAuth; "root_browser_auth_remains_interactive")]
#[test_case(Caller::Root, RequestKind::ShutdownFormInput; "pending_root_form_is_abandoned_before_shutdown")]
#[test_case(Caller::Subagent, RequestKind::FormInput; "subagent_receives_form_input")]
#[test_case(Caller::Subagent, RequestKind::Permission; "subagent_permission_remains_interactive")]
#[test_case(Caller::FullAccessSubagent, RequestKind::Permission; "full_access_subagent_automatically_approves_permission")]
#[test_case(Caller::Subagent, RequestKind::StrictReview; "subagent_keeps_strict_automatic_safety_review")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_elicitations_follow_approval_policy(
    caller: Caller,
    request_kind: RequestKind,
) -> Result<()> {
    custody_environment()?;
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(Ok(()), "the MCP fixture requires a host Python interpreter");
    mcp_server_elicitation_scenario(caller, request_kind).await?;
    Ok(())
}

pub(super) async fn mcp_server_elicitation_scenario(
    caller: Caller,
    request_kind: RequestKind,
) -> Result<Vec<ResponsesRequest>> {
    let custody = custody_environment()?;
    let server = responses::start_mock_server().await;
    let mut elicitation = json!({
        "mode": "form",
        "message": "Provide the requested input.",
        "requestedSchema": {"type": "object", "properties": {}}
    });
    match request_kind {
        RequestKind::BrowserAuth => {
            // Browser credentials travel through the browser broker, so the MCP schema is empty.
            elicitation["_meta"] = json!({
                "codex_approval_kind": "browser_auth",
                "browser_auth_challenge_id": "test_browser_auth_challenge_0123456789",
                "origin": "https://example.com",
                "reason": "Sign in to continue browsing.",
                "fields": [{"id": "password", "label": "Password", "type": "password", "required": true}]
            });
        }
        RequestKind::FormInput | RequestKind::ShutdownFormInput => {
            elicitation["requestedSchema"]["properties"] = json!({"answer": {"type": "string"}});
        }
        RequestKind::Permission | RequestKind::StrictReview => {
            elicitation["_meta"] = json!({
                "codex_request_type": "approval_request",
                "codex_approval_kind": "mcp_tool_call",
                "codex_strict_auto_review": matches!(request_kind, RequestKind::StrictReview),
                "codex_sensitive_action": true,
                "tool_name": "write_record",
                "tool_params": {"value": 42}
            });
        }
    }
    let full_access = matches!(caller, Caller::FullAccessSubagent);
    let expects_prompt = !full_access && !matches!(request_kind, RequestKind::StrictReview);
    let test = test_codex().with_config(move |config| {
        config.approvals_reviewer = ApprovalsReviewer::User;
        config.mcp_servers.set(serde_json::from_value(json!({
            "elicitation": { "command": if cfg!(windows) { "python" } else { "python3" },
                "args": ["-u", "-c", SERVER, elicitation.to_string()], "default_tools_approval_mode": "approve", "env": custody }
        })).expect("MCP fixture configuration")).expect("MCP fixture constraints");
    }).build_with_auto_env(&server).await?;
    let mut owned_child = None;
    let outcome = std::panic::AssertUnwindSafe(async {
        let tool_call = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_function_call_with_namespace(
                    "input-call",
                    "mcp__elicitation",
                    "request_input",
                    "{}",
                ),
                responses::ev_completed("tool-response"),
            ]),
        )
        .await;
        let guardian = if matches!(request_kind, RequestKind::StrictReview) {
            Some(
                responses::mount_sse_once_match(
                    &server,
                    body_partial_json(
                        json!({"client_metadata": {"x-openai-subagent": "guardian"}}),
                    ),
                    responses::sse(vec![
                    responses::ev_assistant_message(
                        "review-result",
                        &json!({
                            "risk_level": "low", "user_authorization": "high", "outcome": "allow",
                            "rationale": "The user requested this action."
                        })
                        .to_string(),
                    ),
                    responses::ev_completed("guardian-review"),
                ]),
                )
                .await,
            )
        } else {
            None
        };
        let follow_up = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("done-message", "done"),
                responses::ev_completed("done-response"),
            ]),
        )
        .await;
        let settings = ThreadSettingsOverrides {
            approval_policy: Some(if full_access {
                AskForApproval::Never
            } else {
                AskForApproval::OnRequest
            }),
            permission_profile: Some(if full_access {
                PermissionProfile::Disabled
            } else {
                PermissionProfile::read_only()
            }),
            ..Default::default()
        };
        let thread = if matches!(caller, Caller::Root) {
            test.codex
                .start_or_steer_turn(
                    TurnInputRequest::user_input(vec![UserInput::Text {
                        text: "Run the input tool.".into(),
                        text_elements: Vec::new(),
                    }])
                    .with_thread_settings(settings),
                )
                .await?;
            std::sync::Arc::clone(&test.codex)
        } else {
            test.spawn_native_subagent(&server, "mcp_elicitation", "Run the input tool.", settings)
                .await?
        };
        if !std::sync::Arc::ptr_eq(&thread, &test.codex) {
            owned_child = Some(std::sync::Arc::clone(&thread));
        }
        let mut startup_ready = false;
        let response_content = if matches!(request_kind, RequestKind::FormInput) {
            json!({"answer": "continue"})
        } else {
            json!({})
        };
        let mut prompts = 0;
        loop {
            let event = wait_for_event(&thread, |event| {
                if let EventMsg::McpStartupComplete(summary) = event {
                    assert!(
                        !summary
                            .failed
                            .iter()
                            .any(|failure| failure.server == "elicitation")
                    );
                    assert!(!summary.cancelled.iter().any(|name| name == "elicitation"));
                    startup_ready = summary.ready.iter().any(|name| name == "elicitation");
                    assert!(startup_ready, "elicitation MCP server must be ready");
                }
                matches!(
                    event,
                    EventMsg::ElicitationRequest(_) | EventMsg::TurnComplete(_)
                )
            })
            .await;
            let EventMsg::ElicitationRequest(request) = event else {
                break;
            };
            assert!(
                expects_prompt,
                "automatically handled elicitations must not prompt the user"
            );
            assert_eq!(prompts, 0, "one tool call must not prompt twice");
            prompts += 1;
            assert!(
                follow_up.requests().is_empty(),
                "the tool must wait for input"
            );
            if matches!(request_kind, RequestKind::ShutdownFormInput) {
                thread.submit(Op::Shutdown).await?;
                let EventMsg::ElicitationAbandoned(abandoned) = wait_for_event(&thread, |event| {
                    matches!(
                        event,
                        EventMsg::ElicitationAbandoned(_) | EventMsg::ShutdownComplete
                    )
                })
                .await
                else {
                    panic!("abandonment must precede shutdown completion");
                };
                assert_eq!(
                    abandoned,
                    codex_protocol::approvals::ElicitationAbandonedEvent {
                        server_name: request.server_name,
                        id: request.id,
                    }
                );
                assert!(matches!(
                    wait_for_event(&thread, |event| {
                        matches!(
                            event,
                            EventMsg::ElicitationAbandoned(_) | EventMsg::ShutdownComplete
                        )
                    })
                    .await,
                    EventMsg::ShutdownComplete
                ));
                assert!(follow_up.requests().is_empty());
                assert!(
                    startup_ready,
                    "actual elicitation server emitted ready summary"
                );
                if !std::sync::Arc::ptr_eq(&thread, &test.codex) {
                    test.codex.shutdown_and_wait().await?;
                }
                return Ok(tool_call.requests());
            }
            thread
                .submit(Op::ResolveElicitation {
                    server_name: request.server_name,
                    request_id: request.id,
                    decision: ElicitationAction::Accept,
                    content: Some(response_content.clone()),
                    meta: None,
                })
                .await?;
        }
        assert!(
            startup_ready,
            "actual elicitation server emitted ready summary"
        );
        assert_eq!(prompts, usize::from(expects_prompt));
        let output = follow_up
            .single_request()
            .function_call_output("input-call");
        let response: Value = serde_json::from_str(
            output["output"][1]["text"]
                .as_str()
                .expect("MCP response must reach the model"),
        )?;
        let expected = if matches!(request_kind, RequestKind::StrictReview) {
            json!({
                "action": "accept", "content": {},
                "_meta": {"approvals_reviewer": "auto_review"}
            })
        } else {
            json!({"action": "accept", "content": response_content})
        };
        assert_eq!(response, expected);
        let mut requests = tool_call.requests();
        if let Some(guardian) = guardian {
            assert!(guardian.single_request().body_contains_text("write_record"));
            requests.extend(guardian.requests());
        }
        requests.extend(follow_up.requests());
        thread.shutdown_and_wait().await?;
        if !std::sync::Arc::ptr_eq(&thread, &test.codex) {
            test.codex.shutdown_and_wait().await?;
        }
        Ok::<Vec<ResponsesRequest>, anyhow::Error>(requests)
    })
    .catch_unwind()
    .await;
    // The exact newly built manager belongs only to this fixture. Retain handles
    // before shutdown, including a child whose setup failed before returning it.
    let mut owned_threads = vec![std::sync::Arc::clone(&test.codex)];
    if let Some(child) = owned_child {
        owned_threads.push(child);
    }
    let mut cleanup_errors = Vec::new();
    for id in test.thread_manager.list_thread_ids().await {
        match test.thread_manager.get_thread(id).await {
            Ok(thread)
                if !owned_threads
                    .iter()
                    .any(|owned| std::sync::Arc::ptr_eq(owned, &thread)) =>
            {
                owned_threads.push(thread);
            }
            Ok(_) => {}
            Err(error) => cleanup_errors.push(format!("owned MCP fixture handle: {error}")),
        }
    }
    let cleanup = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for thread in &owned_threads {
            if let Err(error) = thread.shutdown_and_wait().await {
                cleanup_errors.push(format!("owned MCP fixture shutdown: {error}"));
            }
            let (config, _) = thread.current_mcp_config_and_runtime_context().await;
            if thread
                .mcp_connection_statuses(&config)
                .await
                .values()
                .any(|status| *status != codex_protocol::mcp::McpServerConnectionStatus::Cancelled)
            {
                cleanup_errors.push("owned MCP fixture client was not cancelled".to_string());
            }
        }
    })
    .await;
    if cleanup.is_err() {
        cleanup_errors
            .push("owned MCP fixture cleanup deadline exceeded; ownership unresolved".into());
    }
    if !cleanup_errors.is_empty() {
        eprintln!("MCP_INTEGRATION_CLEANUP_UNQUALIFIED: {cleanup_errors:?}");
    }
    match outcome {
        Err(panic) => std::panic::resume_unwind(panic),
        Ok(result) => {
            let value = result?;
            anyhow::ensure!(
                cleanup_errors.is_empty(),
                "MCP fixture cleanup failed: {cleanup_errors:?}"
            );
            eprintln!(
                "MCP_INTEGRATION_OWNED_CLEANUP_COMPLETE threads={}",
                owned_threads.len()
            );
            Ok(value)
        }
    }
}
