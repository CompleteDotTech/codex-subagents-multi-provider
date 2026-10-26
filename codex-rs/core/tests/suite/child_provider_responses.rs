//! Actual Session/tool execution against two configured stateless providers.
//! Native spawn authorization and persistence are qualified by their own tasks.
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderCapabilities;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::ResponsesCompatibility;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn compatible(name: &'static str, base_url: String, retries: u64) -> TestCodexBuilder {
    test_codex()
        .with_model("portable-model")
        .with_model_info_override("portable-model", |model| {
            model.tool_mode = Some(ToolMode::Direct);
            model.use_responses_lite = false;
        })
        .with_config(move |config| {
            for feature in [
                Feature::CodeMode,
                Feature::CodeModeOnly,
                Feature::ToolSearch,
                Feature::CodeModeToolSearch,
            ] {
                config.features.disable(feature).unwrap();
            }
            config.model_provider_id = name.into();
            config.model_provider = ModelProviderInfo {
                name: name.into(),
                base_url: Some(base_url),
                capabilities: Some(ModelProviderCapabilities {
                    responses: Some(ResponsesCompatibility::StatelessFunctions),
                    ..Default::default()
                }),
                // Even a mistaken WS opt-in cannot enable stateful continuation.
                supports_websockets: true,
                request_max_retries: Some(0),
                stream_max_retries: Some(retries),
                stream_idle_timeout_ms: Some(250),
                ..Default::default()
            };
        })
}

async fn turn(
    test: &TestCodex,
    text: &str,
) -> anyhow::Result<Option<codex_protocol::protocol::ErrorEvent>> {
    let (sandbox_policy, permission_profile) =
        core_test_support::test_codex::turn_permission_fields(
            codex_protocol::models::PermissionProfile::Disabled,
            test.cwd.path(),
        );
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            ),
        )
        .await?;
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(30),
    )
    .await;
    let EventMsg::TurnComplete(completed) = event else {
        unreachable!()
    };
    if let Some(error) = &completed.error {
        eprintln!("turn {text:?} failed: {error:?}");
    }
    Ok(completed.error)
}

fn completion(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message("answer", "done"),
        ev_completed(id),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_stateless_providers_complete_real_patch_tool_round_trips_and_preserve_parent()
-> anyhow::Result<()> {
    let parent_server = start_mock_server().await;
    mount_sse_sequence(
        &parent_server,
        vec![completion("parent-before"), completion("parent-after")],
    )
    .await;
    let mut parent_builder = test_codex();
    let parent = parent_builder.build_with_auto_env(&parent_server).await?;
    assert!(turn(&parent, "before children").await?.is_none());
    for name in ["configured-compatible-a", "configured-compatible-b"] {
        let server = start_mock_server().await;
        let patch =
            "*** Begin Patch\n*** Add File: compatible.txt\n+exact raw content\n*** End Patch\n";
        let first = sse(vec![
            ev_response_created("tool-turn"),
            ev_function_call(
                "patch-call",
                "apply_patch",
                &json!({"input":patch}).to_string(),
            ),
            ev_completed("tool-turn"),
        ]);
        mount_sse_sequence(
            &server,
            vec![
                first,
                completion("after-tool"),
                completion("subsequent-turn"),
            ],
        )
        .await;
        let mut builder = compatible(name, format!("{}/v1", server.uri()), 0);
        let test = builder.build_with_auto_env(&server).await?;
        assert!(turn(&test, "apply the requested patch").await?.is_none());
        assert_eq!(
            tokio::fs::read_to_string(test.cwd.path().join("compatible.txt")).await?,
            "exact raw content\n"
        );
        assert!(
            turn(&test, "continue with portable history")
                .await?
                .is_none()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.url.path(), "/v1/responses");
            let body: Value = request.body_json()?;
            assert_eq!(body["model"], "portable-model");
            assert!(body.get("previous_response_id").is_none());
            assert!(body.get("conversation").is_none());
            assert!(body["include"].as_array().unwrap().is_empty());
            assert!(body.get("prompt_cache_key").is_none());
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|tool| tool["type"] == "function")
            );
            assert!(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item.get("id").is_none()
                        && item["type"] != "reasoning"
                        && item["type"] != "custom_tool_call"
                        && item["type"] != "custom_tool_call_output")
            );
        }
        let replay: Value = requests[1].body_json()?;
        let call = replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call" && item["call_id"] == "patch-call")
            .expect("actual tool call replay");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap())?,
            json!({"input":patch})
        );
        let result = replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "patch-call")
            .expect("actual handler result replay");
        assert!(result["output"].as_str().unwrap().contains("Success"));
        let patch_spec = replay["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "apply_patch")
            .unwrap();
        assert_eq!(patch_spec["parameters"]["required"], json!(["input"]));
        assert_eq!(test.config.model_provider_id, name);
    }
    assert!(turn(&parent, "after children").await?.is_none());
    assert_eq!(parent.config.model_provider_id, "openai");
    assert_eq!(parent_server.received_requests().await.unwrap().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_streams_exhaust_retries_on_the_selected_provider_without_success()
-> anyhow::Result<()> {
    let other = start_mock_server().await;
    for name in ["configured-compatible-a", "configured-compatible-b"] {
        for body in [
            "data: {malformed\n\n".to_string(),
            sse(vec![
                json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"unfinished","name":"apply_patch","arguments":""}}),
                ev_completed("not-success"),
            ]),
        ] {
            let server = start_mock_server().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .expect(2)
                .mount(&server)
                .await;
            let mut builder = compatible(name, format!("{}/v1", server.uri()), 1);
            let test = builder.build_with_auto_env(&server).await?;
            assert!(turn(&test, "fail incomplete response").await?.is_some());
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
            assert_eq!(test.config.model_provider_id, name);
        }
    }
    assert!(other.received_requests().await.unwrap().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_freeform_wrappers_and_invalid_patches_use_original_validation()
-> anyhow::Result<()> {
    for (arguments, expected) in [
        (
            json!({"input":"*** Begin Patch\n*** Add File: forbidden.txt\n+bad\n*** End Patch\n","extra":true}),
            "only a string input field",
        ),
        (
            json!({"input":"not a patch"}),
            "apply_patch verification failed",
        ),
    ] {
        let server = start_mock_server().await;
        mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("invalid-tool"),
                    ev_function_call("invalid-call", "apply_patch", &arguments.to_string()),
                    ev_completed("invalid-tool"),
                ]),
                completion("after-validation"),
            ],
        )
        .await;
        let mut builder = compatible("configured-validation", format!("{}/v1", server.uri()), 0);
        let test = builder.build_with_auto_env(&server).await?;
        assert!(
            turn(&test, "reject the invalid tool arguments")
                .await?
                .is_none()
        );
        assert!(!test.cwd.path().join("forbidden.txt").exists());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let body: Value = requests[1].body_json()?;
        let output = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "invalid-call"
            })
            .unwrap();
        assert!(output["output"].as_str().unwrap().contains(expected));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_http_200_quota_and_interrupted_streams_fail_the_turn() -> anyhow::Result<()> {
    for (content_type, body, quota) in [
        (
            "application/json",
            json!({"error":{"code":"insufficient_quota","message":"quota"}}).to_string(),
            true,
        ),
        (
            "text/event-stream",
            sse(vec![
                json!({"type":"response.incomplete","response":{"id":"partial","incomplete_details":{"reason":"interrupted"}}}),
            ]),
            false,
        ),
    ] {
        let server = start_mock_server().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
            .mount(&server)
            .await;
        let mut builder = compatible("configured-errors", format!("{}/v1", server.uri()), 0);
        let test = builder.build_with_auto_env(&server).await?;
        let error = turn(&test, "report a failure").await?.expect("failed turn");
        if quota {
            assert_eq!(
                error.codex_error_info,
                Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded),
                "structured quota must retain its typed category: {error:?}"
            );
        } else {
            assert!(error.message.contains("interrupted"), "{error:?}");
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_stops_the_selected_request_without_tool_execution() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(10))
                .set_body_string(completion("too-late")),
        )
        .mount(&server)
        .await;
    let mut builder = compatible("configured-cancel", format!("{}/v1", server.uri()), 0);
    let test = builder.build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "cancel before response".into(),
            text_elements: vec![],
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnAborted(_)),
        Duration::from_secs(5),
    )
    .await;
    assert!(!test.cwd.path().join("compatible.txt").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    Ok(())
}
