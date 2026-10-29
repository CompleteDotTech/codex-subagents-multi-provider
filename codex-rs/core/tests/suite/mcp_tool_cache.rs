use anyhow::Context;
use codex_config::Constrained;
use codex_config::types::McpServerConfig;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::mount_sse_once;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::test_env;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::rmcp_client::remote_aware_environment_id;
use super::rmcp_client::remote_aware_stdio_server_bin;

const SERVER_NAME: &str = "cached_rmcp";
const NAMESPACE: &str = "mcp__cached_rmcp";

fn user_turn(prompt: &str) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: prompt.to_string(),
        text_elements: Vec::new(),
    }])
    .with_thread_settings(ThreadSettingsOverrides {
        approval_policy: Some(AskForApproval::Never),
        permission_profile: Some(PermissionProfile::Disabled),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_calls_stay_bound_to_each_thread() -> anyhow::Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let responses_server = responses::start_mock_server().await;
    let command = remote_aware_stdio_server_bin()?;
    let environment_id = remote_aware_environment_id();
    let test_env = test_env().await?;
    let make_server = |marker| {
        serde_json::from_value::<McpServerConfig>(json!({
            "command": command,
            "environment_id": environment_id,
            "cwd": test_env.cwd(),
            "env": {
                "MCP_TEST_DYNAMIC_SERVER_METADATA": "1",
                "MCP_TEST_VALUE": marker,
            },
            "enabled_tools": ["echo"],
            "startup_timeout_sec": 10,
        }))
    };
    let first_server = make_server("first-runtime")?;
    let second_server = make_server("second-runtime")?;
    let fixture = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
        .with_config(move |config| {
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .expect("first thread should accept disabled permissions");
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(SERVER_NAME.to_string(), first_server);
            config
                .mcp_servers
                .set(servers)
                .expect("first thread should accept its MCP servers");
        })
        .build_with_environment(&responses_server, test_env)
        .await?;

    let mut second_config = fixture.config.clone();
    let mut second_servers = second_config.mcp_servers.get().clone();
    second_servers.insert(SERVER_NAME.to_string(), second_server);
    second_config.mcp_servers.set(second_servers)?;
    let NewThread {
        thread: second_thread,
        ..
    } = fixture
        .thread_manager
        .start_thread(StartThreadOptions::new(second_config))
        .await?;

    wait_for_mcp_server(&fixture.codex, SERVER_NAME).await?;
    wait_for_mcp_server(&second_thread, SERVER_NAME).await?;

    let calls = [
        (&fixture.codex, "first-call", "first-runtime"),
        (&second_thread, "second-call", "second-runtime"),
        (&fixture.codex, "first-again", "first-runtime"),
    ];
    let mut processes = Vec::new();
    for (thread, call_id, marker) in calls {
        let call_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created(call_id),
                responses::ev_function_call_with_namespace(
                    call_id,
                    NAMESPACE,
                    "echo",
                    &json!({ "message": call_id }).to_string(),
                ),
                responses::ev_completed(call_id),
            ]),
        )
        .await;
        let completion_response = mount_sse_once(
            &responses_server,
            responses::sse(vec![
                responses::ev_response_created(&format!("{call_id}-done")),
                responses::ev_assistant_message(call_id, "done"),
                responses::ev_completed(&format!("{call_id}-done")),
            ]),
        )
        .await;
        thread
            .start_or_steer_turn(user_turn(&format!("Call the {SERVER_NAME} echo tool.")))
            .await?;
        let EventMsg::McpToolCallEnd(end) = wait_for_event(
            thread,
            |event| matches!(event, EventMsg::McpToolCallEnd(end) if end.call_id == call_id),
        )
        .await
        else {
            unreachable!("event predicate guarantees the requested MCP result");
        };
        assert_eq!(
            end.invocation,
            McpInvocation {
                server: SERVER_NAME.to_string(),
                tool: "echo".to_string(),
                arguments: Some(json!({ "message": call_id })),
            }
        );
        let content = end
            .result
            .expect("thread-local MCP call should succeed")
            .structured_content
            .expect("echo should return structured content");
        let process = content
            .get("echo")
            .and_then(Value::as_str)
            .expect("echo should identify its server process")
            .to_string();
        assert!(process.starts_with("rmcp-test-process-"));
        assert_eq!(content, json!({ "echo": process, "env": marker }));
        wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        let request = call_response.single_request();
        assert!(request.tool_by_name(NAMESPACE, "echo").is_some());
        let completion_request = completion_response.single_request();
        assert_eq!(
            request.body_json()["tools"],
            completion_request.body_json()["tools"],
            "MCP tool schemas must remain unchanged across a same-turn continuation"
        );
        let output = completion_request
            .function_call_output_text(call_id)
            .expect("MCP result should be returned to the model");
        assert!(output.contains(&process));
        assert!(output.contains(marker));
        processes.push(process);
    }

    assert_ne!(processes[0], processes[1]);
    assert_eq!(processes[0], processes[2]);

    fixture.codex.shutdown_and_wait().await?;
    second_thread.shutdown_and_wait().await?;
    responses_server.verify().await;
    Ok(())
}
