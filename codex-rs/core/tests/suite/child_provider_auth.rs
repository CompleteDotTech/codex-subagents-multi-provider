//! Session/ModelClient boundary tests. Native selector exposure is covered separately.
use codex_core::TurnInputRequest;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::start_websocket_server_with_headers;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use http::HeaderMap;
use http::HeaderValue;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing_test::traced_test;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

struct HostAuth {
    auth: CodexAuth,
    refreshes: AtomicUsize,
}

impl ExternalAuth for HostAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.auth.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.auth.clone()) })
    }
}

fn rotating_helper() -> anyhow::Result<(tempfile::TempDir, ModelProviderAuthInfo)> {
    let home = tempfile::tempdir()?;
    #[cfg(unix)]
    let (command, args) = {
        let script = home.path().join("helper.sh");
        std::fs::write(
            &script,
            concat!(
                "printf 'invoked\\n' >> calls\n",
                "if [ -f invoked ]; then printf synthetic-command-refreshed; else touch invoked; printf synthetic-command-initial; fi\n",
            ),
        )?;
        ("sh", vec![script.to_string_lossy().into_owned()])
    };
    #[cfg(windows)]
    let (command, args) = {
        let script = home.path().join("helper.cmd");
        std::fs::write(
            &script,
            concat!(
                "@echo off\r\n>>calls echo invoked\r\n",
                "if exist invoked (echo synthetic-command-refreshed& exit /b 0)\r\n",
                "echo invoked>invoked\r\necho synthetic-command-initial\r\n",
            ),
        )?;
        (
            "cmd.exe",
            vec![
                "/D".into(),
                "/C".into(),
                script.to_string_lossy().into_owned(),
            ],
        )
    };
    let config = serde_json::from_value(serde_json::json!({
        "command": command, "args": args, "cwd": home.path(),
        "timeout_ms": 5_000, "refresh_interval_ms": 0,
    }))?;
    Ok((home, config))
}

fn static_models() -> Arc<StaticModelsManager> {
    Arc::new(StaticModelsManager::new(
        None,
        codex_models_manager::bundled_models_response().unwrap(),
    ))
}

fn shared_host() -> Arc<HostAuth> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer synthetic-parent-token"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("parent-account"),
    );
    headers.insert(
        "x-openai-actor-id",
        HeaderValue::from_static("parent-actor"),
    );
    Arc::new(HostAuth {
        auth: CodexAuth::Headers(AuthHeaders::new(headers)),
        refreshes: AtomicUsize::new(0),
    })
}

async fn successful_turn(test: &TestCodex, prompt: &str) -> anyhow::Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: vec![],
        }]))
        .await?;
    let EventMsg::TurnComplete(completed) = wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(30),
    )
    .await
    else {
        unreachable!("predicate requires turn completion");
    };
    assert!(
        completed.error.is_none(),
        "synthetic turn must complete successfully"
    );
    Ok(())
}

async fn failed_turn(test: &TestCodex, prompt: &str) -> anyhow::Result<ErrorEvent> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: vec![],
        }]))
        .await?;
    let mut emitted_error = None;
    let EventMsg::TurnComplete(completed) = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            if let EventMsg::Error(error) = event {
                emitted_error = Some(error.clone());
            }
            matches!(event, EventMsg::TurnComplete(_))
        },
        Duration::from_secs(30),
    )
    .await
    else {
        unreachable!("predicate requires turn completion");
    };
    let error = completed
        .error
        .expect("rejected request must fail the turn");
    assert_eq!(Some(error.clone()), emitted_error);
    let diagnostic = serde_json::to_string(&error)?;
    for secret in [
        "synthetic-parent-token",
        "synthetic-child-token",
        "parent-account",
        "parent-actor",
    ] {
        assert!(
            !diagnostic.contains(secret),
            "terminal failure must omit credentials and foreign identity"
        );
    }
    Ok(error)
}

fn blocking_helper(refresh: bool) -> anyhow::Result<(tempfile::TempDir, ModelProviderAuthInfo)> {
    let home = tempfile::tempdir()?;
    let mode = if refresh { "refresh" } else { "loading" };
    #[cfg(unix)]
    let (command, args) = {
        let script = home.path().join("blocking.sh");
        std::fs::write(
            &script,
            concat!(
                "if [ \"$1\" = loading ] || [ -f invoked ]; then\n",
                "touch started\nwhile [ ! -f release ]; do sleep 0.01; done\nfi\n",
                "touch invoked\nprintf synthetic-blocking-key\n",
            ),
        )?;
        (
            "sh",
            vec![script.to_string_lossy().into_owned(), mode.to_string()],
        )
    };
    #[cfg(windows)]
    let (command, args) = {
        let script = home.path().join("blocking.cmd");
        std::fs::write(
            &script,
            concat!(
                "@echo off\r\nif \"%~1\"==\"loading\" goto wait\r\nif exist invoked goto wait\r\ngoto token\r\n",
                ":wait\r\necho started>started\r\n:poll\r\nif not exist release goto poll\r\n",
                ":token\r\necho invoked>invoked\r\necho synthetic-blocking-key\r\n",
            ),
        )?;
        (
            "cmd.exe",
            vec![
                "/D".into(),
                "/C".into(),
                script.to_string_lossy().into_owned(),
                mode.to_string(),
            ],
        )
    };
    let auth = serde_json::from_value(serde_json::json!({
        "command": command, "args": args, "cwd": home.path(),
        "timeout_ms": 30_000, "refresh_interval_ms": 0,
    }))?;
    Ok((home, auth))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_helper_loading_and_refresh_sends_no_post_cancel_request()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    for refresh in [false, true] {
        let parent_server = start_mock_server().await;
        let child_server = start_mock_server().await;
        let completion = || sse(vec![ev_response_created("r"), ev_completed("r")]);
        let parent_requests = mount_sse_sequence(&parent_server, vec![completion()]).await;
        if refresh {
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(401))
                .with_priority(1)
                .up_to_n_times(2)
                .mount(&child_server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(completion()),
            )
            .mount(&child_server)
            .await;
        let host = shared_host();
        let manager = AuthManager::from_auth_for_testing(host.auth.clone());
        manager.set_external_auth(host.clone()).await?;
        let mut parent_builder = test_codex().with_auth_manager(manager.clone());
        let parent = parent_builder.build_with_auto_env(&parent_server).await?;
        let (helper_home, auth) = blocking_helper(refresh)?;
        let base_url = format!("{}/v1", child_server.uri());
        let mut child_builder = test_codex()
            .with_auth_manager(manager.clone())
            .with_models_manager(static_models())
            .with_config(move |config| {
                config.model_provider_id = "cancellable-command-child".into();
                config.model_provider = ModelProviderInfo {
                    name: "cancellable-command-child".into(),
                    base_url: Some(base_url),
                    auth: Some(auth),
                    requires_openai_auth: false,
                    wire_api: WireApi::Responses,
                    request_max_retries: Some(0),
                    stream_max_retries: Some(0),
                    ..Default::default()
                };
            });
        let child = child_builder.build_with_auto_env(&child_server).await?;
        child
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "wait in credential helper".into(),
                text_elements: vec![],
            }]))
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !helper_home.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let expected_before_retry = usize::from(refresh);
        assert_eq!(
            child_server.received_requests().await.unwrap().len(),
            expected_before_retry
        );
        child.codex.submit(Op::Interrupt).await?;
        let aborted = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_event(&child.codex, |event| {
                matches!(event, EventMsg::TurnAborted(_))
            }),
        )
        .await?;
        let EventMsg::TurnAborted(aborted) = aborted else {
            unreachable!()
        };
        assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
        assert!(aborted.error.is_none());
        std::fs::write(helper_home.path().join("release"), [])?;
        successful_turn(&parent, "parent remains usable after child cancellation").await?;
        assert_eq!(
            child_server.received_requests().await.unwrap().len(),
            expected_before_retry
        );
        // A later turn must not remain stuck behind the cancelled helper's cache/refresh locks.
        // In the refresh case it receives another 401 and must perform a fresh recovery.
        tokio::time::timeout(
            Duration::from_secs(10),
            successful_turn(&child, "child resumes after cancellation"),
        )
        .await??;
        assert_eq!(
            child_server.received_requests().await.unwrap().len(),
            expected_before_retry + if refresh { 2 } else { 1 }
        );
        assert_eq!(
            child.session_configured.model_provider_id,
            "cancellable-command-child"
        );
        assert_eq!(parent_requests.requests().len(), 1);
        assert_eq!(host.refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(manager.auth().await, Some(host.auth.clone()));
        parent.codex.shutdown_and_wait().await?;
        child.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_child_401_refreshes_only_its_helper_through_model_client() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let parent_server = start_mock_server().await;
    let child_server = start_mock_server().await;
    let completion = || sse(vec![ev_response_created("r"), ev_completed("r")]);
    let parent_requests =
        mount_sse_sequence(&parent_server, vec![completion(), completion()]).await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer synthetic-command-initial"))
        .respond_with(ResponseTemplate::new(401).set_body_string("synthetic unauthorized"))
        .expect(1)
        .mount(&child_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header(
            "authorization",
            "Bearer synthetic-command-refreshed",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completion()),
        )
        .expect(2)
        .mount(&child_server)
        .await;
    let (helper_home, auth) = rotating_helper()?;
    let host = shared_host();
    let manager = AuthManager::from_auth_for_testing(host.auth.clone());
    manager.set_external_auth(host.clone()).await?;
    let mut parent_builder = test_codex().with_auth_manager(manager.clone());
    let parent = parent_builder.build_with_auto_env(&parent_server).await?;
    let base_url = format!("{}/v1", child_server.uri());
    let mut child_builder = test_codex()
        .with_auth_manager(manager.clone())
        .with_models_manager(static_models())
        .with_config(move |config| {
            config.model_provider_id = "command-child".into();
            config.model_provider = ModelProviderInfo {
                name: "command-child".into(),
                base_url: Some(base_url),
                auth: Some(auth),
                requires_openai_auth: false,
                wire_api: WireApi::Responses,
                request_max_retries: Some(0),
                stream_max_retries: Some(0),
                ..Default::default()
            };
        });
    let child = child_builder.build_with_auto_env(&child_server).await?;
    let (parent_result, child_result) = tokio::join!(
        successful_turn(&parent, "parent"),
        successful_turn(&child, "command refresh")
    );
    parent_result?;
    child_result?;
    successful_turn(&child, "reuse refreshed command token").await?;
    successful_turn(&parent, "parent after child refresh").await?;
    child_server.verify().await;
    let requests = child_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for (request, token) in requests.iter().zip([
        "Bearer synthetic-command-initial",
        "Bearer synthetic-command-refreshed",
        "Bearer synthetic-command-refreshed",
    ]) {
        assert_eq!(request.headers["authorization"], token);
        assert_eq!(
            request.body_json::<serde_json::Value>()?["model"],
            child.session_configured.model
        );
        for name in [
            "chatgpt-account-id",
            "x-openai-actor-id",
            "x-openai-account-routing-override",
        ] {
            assert!(!request.headers.contains_key(name));
        }
    }
    assert_eq!(
        std::fs::read_to_string(helper_home.path().join("calls"))?
            .lines()
            .count(),
        2
    );
    assert_eq!(parent_requests.requests().len(), 2);
    assert_eq!(host.refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(manager.auth().await, Some(host.auth.clone()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_child_reconnect_keeps_parent_and_local_connections_isolated()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let completion = || vec![ev_response_created("r"), ev_completed("r")];
    let connection = |requests| WebSocketConnectionConfig {
        requests,
        response_headers: vec![],
        accept_delay: None,
        close_after_requests: false,
    };
    let parent_server =
        start_websocket_server_with_headers(vec![connection(vec![completion(), completion()])])
            .await;
    let local_server =
        start_websocket_server_with_headers(vec![connection(vec![completion(), completion()])])
            .await;
    let mut rejected_connection = connection(vec![vec![
        serde_json::json!({"type": "error", "status": 400, "error": {
            "type": "invalid_request_error", "code": "websocket_connection_limit_reached",
            "message": "synthetic reconnect",
        }}),
    ]]);
    rejected_connection.close_after_requests = true;
    let child_server = start_websocket_server_with_headers(vec![
        rejected_connection,
        connection(vec![completion(), completion()]),
    ])
    .await;
    let (helper_home, auth) = rotating_helper()?;
    let host = shared_host();
    let manager = AuthManager::from_auth_for_testing(host.auth.clone());
    manager.set_external_auth(host.clone()).await?;
    let mut parent_builder = test_codex().with_auth_manager(manager.clone());
    let parent = parent_builder
        .build_with_websocket_server(&parent_server)
        .await?;
    let child_url = format!("{}/v1", child_server.uri());
    let mut child_builder = test_codex()
        .with_auth_manager(manager.clone())
        .with_models_manager(static_models())
        .with_config(move |config| {
            config.model_provider_id = "command-websocket-child".into();
            config.model_provider = ModelProviderInfo {
                name: "command-websocket-child".into(),
                base_url: Some(child_url),
                auth: Some(auth),
                requires_openai_auth: false,
                wire_api: WireApi::Responses,
                request_max_retries: Some(0),
                stream_max_retries: Some(1),
                ..Default::default()
            };
        });
    let child = child_builder
        .build_with_websocket_server(&child_server)
        .await?;
    let local_url = format!("{}/v1", local_server.uri());
    let mut local_builder = test_codex()
        .with_auth_manager(manager.clone())
        .with_models_manager(static_models())
        .with_config(move |config| {
            config.model_provider_id = "local-websocket-child".into();
            config.model_provider = ModelProviderInfo {
                name: "local-websocket-child".into(),
                base_url: Some(local_url),
                requires_openai_auth: false,
                wire_api: WireApi::Responses,
                request_max_retries: Some(0),
                stream_max_retries: Some(0),
                ..Default::default()
            };
        });
    let local = local_builder
        .build_with_websocket_server(&local_server)
        .await?;
    let (a, b, c) = tokio::join!(
        successful_turn(&parent, "parent"),
        successful_turn(&child, "child reconnect"),
        successful_turn(&local, "local")
    );
    a?;
    b?;
    c?;
    let (a, b, c) = tokio::join!(
        successful_turn(&parent, "parent still connected"),
        successful_turn(&child, "child reuses socket"),
        successful_turn(&local, "local still connected")
    );
    a?;
    b?;
    c?;
    assert_eq!(parent_server.handshakes().len(), 1);
    assert_eq!(local_server.handshakes().len(), 1);
    assert_eq!(child_server.handshakes().len(), 2);
    assert_eq!(parent_server.single_connection().len(), 2);
    assert_eq!(local_server.single_connection().len(), 2);
    assert_eq!(
        child_server
            .connections()
            .iter()
            .map(Vec::len)
            .sum::<usize>(),
        3
    );
    assert_eq!(
        parent_server
            .single_handshake()
            .header("authorization")
            .as_deref(),
        Some("Bearer synthetic-parent-token")
    );
    for (server, token) in [
        (&child_server, Some("Bearer synthetic-command-initial")),
        (&local_server, None),
    ] {
        for handshake in server.handshakes() {
            assert_eq!(handshake.uri(), "/v1/responses");
            assert_eq!(handshake.header("authorization").as_deref(), token);
            for name in [
                "chatgpt-account-id",
                "x-openai-actor-id",
                "x-openai-account-routing-override",
                "x-codex-routing-hint",
            ] {
                assert_eq!(handshake.header(name), None);
            }
        }
        for request in server.connections().into_iter().flatten() {
            assert_eq!(request.body_json()["model"], "gpt-5.5");
        }
    }
    assert_eq!(
        std::fs::read_to_string(helper_home.path().join("calls"))?
            .lines()
            .count(),
        1
    );
    assert_eq!(host.refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(manager.auth().await, Some(host.auth.clone()));
    parent.codex.shutdown_and_wait().await?;
    child.codex.shutdown_and_wait().await?;
    local.codex.shutdown_and_wait().await?;
    parent_server.shutdown().await;
    child_server.shutdown().await;
    local_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[traced_test]
async fn shared_host_auth_remains_usable_after_independent_session_failures() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));
    let parent_server = start_mock_server().await;
    let child_server = start_mock_server().await;
    let local_server = start_mock_server().await;
    let completion = || sse(vec![ev_response_created("r"), ev_completed("r")]);
    let parent_requests =
        mount_sse_sequence(&parent_server, vec![completion(), completion()]).await;
    let local_requests = mount_sse_sequence(&local_server, vec![completion(), completion()]).await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({"error": {"message": "synthetic rejection"}})),
        )
        .mount(&child_server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer synthetic-parent-token"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("parent-account"),
    );
    headers.insert(
        "x-openai-actor-id",
        HeaderValue::from_static("parent-actor"),
    );
    let host_auth = Arc::new(HostAuth {
        auth: CodexAuth::Headers(AuthHeaders::new(headers)),
        refreshes: AtomicUsize::new(0),
    });
    let manager = AuthManager::from_auth_for_testing(host_auth.auth.clone());
    manager.set_external_auth(host_auth.clone()).await?;
    let mut parent_builder = test_codex().with_auth_manager(manager.clone());
    let parent = parent_builder.build_with_auto_env(&parent_server).await?;
    let keyed = ModelProviderInfo {
        name: "independent-keyed".into(),
        base_url: Some(format!("{}/v1", child_server.uri())),
        requires_openai_auth: false,
        wire_api: WireApi::Responses,
        experimental_bearer_token: Some("synthetic-child-token".into()),
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        ..Default::default()
    };
    let mut local = keyed.clone();
    local.name = "independent-local".into();
    local.base_url = Some(format!("{}/v1", local_server.uri()));
    local.experimental_bearer_token = None;
    let mut child_builder =
        test_codex()
            .with_auth_manager(manager.clone())
            .with_config(move |config| {
                config.model_provider_id = keyed.name.clone();
                config.model_provider = keyed;
            });
    let child = child_builder.build_with_auto_env(&child_server).await?;
    let mut local_builder =
        test_codex()
            .with_auth_manager(manager.clone())
            .with_config(move |config| {
                config.model_provider_id = local.name.clone();
                config.model_provider = local;
            });
    let local = local_builder.build_with_auto_env(&local_server).await?;
    let (a, b, c) = tokio::join!(
        successful_turn(&parent, "parent"),
        failed_turn(&child, "child rejection"),
        successful_turn(&local, "local")
    );
    a?;
    let unauthorized = b?;
    assert_eq!(
        unauthorized.codex_error_info,
        Some(
            codex_protocol::protocol::CodexErrorInfo::HttpConnectionFailed {
                http_status_code: Some(401)
            }
        )
    );
    assert_eq!(
        child.session_configured.model_provider_id,
        "independent-keyed"
    );
    c?;
    let mut child_requests = child_server.received_requests().await.unwrap();
    assert_eq!(child_requests.len(), 1);
    child_server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(serde_json::json!({"error": {"type": "insufficient_quota", "message": "synthetic quota"}})),
        )
        .mount(&child_server)
        .await;
    let quota = failed_turn(&child, "child quota").await?;
    assert_eq!(
        quota.codex_error_info,
        Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded)
    );
    assert_eq!(
        child.session_configured.model_provider_id,
        "independent-keyed"
    );
    successful_turn(&parent, "parent still works").await?;
    successful_turn(&local, "local still works").await?;
    assert_eq!(host_auth.refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(manager.auth().await, Some(host_auth.auth.clone()));
    for request in parent_requests.requests() {
        assert_eq!(
            request.header("authorization").as_deref(),
            Some("Bearer synthetic-parent-token")
        );
    }
    for request in local_requests.requests() {
        for name in [
            "authorization",
            "chatgpt-account-id",
            "x-openai-actor-id",
            "x-openai-account-routing-override",
        ] {
            assert_eq!(request.header(name), None);
        }
        assert_eq!(request.body_json()["model"], local.session_configured.model);
    }
    child_requests.extend(child_server.received_requests().await.unwrap());
    assert_eq!(child_requests.len(), 2);
    assert_eq!(parent_requests.requests().len(), 2);
    assert_eq!(local_requests.requests().len(), 2);
    for request in child_requests {
        assert_eq!(
            request.headers["authorization"],
            "Bearer synthetic-child-token"
        );
        assert!(!request.headers.contains_key("chatgpt-account-id"));
        assert!(!request.headers.contains_key("x-openai-actor-id"));
    }
    logs_assert(|lines: &[&str]| {
        if lines.is_empty() {
            return Err("request-failure fixture must capture runtime diagnostics".into());
        }
        for secret in ["synthetic-parent-token", "synthetic-child-token"] {
            if lines.iter().any(|line| line.contains(secret)) {
                return Err(
                    "request diagnostics exposed a synthetic credential or foreign identity".into(),
                );
            }
        }
        Ok(())
    });
    Ok(())
}
