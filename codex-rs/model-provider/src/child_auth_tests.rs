//! Provider instances built with the same host manager must isolate inference auth.
use super::*;
use codex_api::Compression;
use codex_api::ResponsesClient;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::ReqwestTransport;
use codex_login::AuthHeaders;
use codex_login::default_client::create_client_for_route;
use codex_model_provider_info::GatewayOAuthConfig;
use codex_model_provider_info::GatewayOAuthDelivery;
use codex_model_provider_info::WireApi;
use http::HeaderMap;
use http::HeaderValue;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn independent_provider(server: &MockServer) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "independent".into(),
        base_url: Some(server.uri()),
        requires_openai_auth: false,
        wire_api: WireApi::Responses,
        request_max_retries: Some(0),
        ..Default::default()
    }
}

fn host_auth() -> CodexAuth {
    CodexAuth::Headers(AuthHeaders::new(HeaderMap::from_iter([
        (
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic-host-token"),
        ),
        (
            http::HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("host-account"),
        ),
        (
            http::HeaderName::from_static("x-openai-actor-id"),
            HeaderValue::from_static("host-actor"),
        ),
    ])))
}

async fn send(provider: &SharedModelProvider, model: &str) -> Result<(), ApiError> {
    let api = provider.api_provider().await.unwrap();
    let auth = provider.api_auth().await.unwrap();
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let transport = ReqwestTransport::from_http_client(
        create_client_for_route(
            &factory,
            &api.url_for_path("responses"),
            ClientRouteClass::Api,
            ClientRedirectPolicy::Default,
        )
        .unwrap(),
    );
    ResponsesClient::new(transport, api, auth)
        .stream(
            json!({"model": model, "input": []}),
            HeaderMap::new(),
            Compression::None,
            None,
        )
        .await
        .map(|_| ())
}

async fn respond(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n",
                ),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn simultaneous_providers_keep_credentials_models_and_failures_isolated() {
    let host = AuthManager::from_auth_for_testing(host_auth());
    let parent_server = MockServer::start().await;
    let keyed_server = MockServer::start().await;
    let local_server = MockServer::start().await;
    respond(&parent_server, 200).await;
    respond(&keyed_server, 401).await;
    respond(&local_server, 200).await;
    let parent = create_model_provider(
        ModelProviderInfo::create_openai_provider(Some(parent_server.uri())),
        Some(host.clone()),
    );
    let mut keyed_info = independent_provider(&keyed_server);
    keyed_info.experimental_bearer_token = Some("synthetic-child-token".into());
    // An explicit key overrides even an explicitly enabled first-party default.
    keyed_info.requires_openai_auth = true;
    let keyed = create_model_provider(keyed_info, Some(host.clone()));
    let local = create_model_provider(independent_provider(&local_server), Some(host.clone()));
    assert!(Arc::ptr_eq(&parent.auth_manager().unwrap(), &host));
    for child in [&keyed, &local] {
        assert!(child.auth_manager().is_none());
        assert!(child.auth().await.is_none());
        assert!(!child.supports_attestation());
    }
    let scoped = local
        .api_auth_for_scope(ProviderAuthScope {
            agent_identity_policy: codex_login::auth::AgentIdentityAuthPolicy::JwtOnly,
            session_source: codex_protocol::protocol::SessionSource::Cli,
            agent_identity_session_fallback: crate::AgentIdentitySessionFallback::default(),
        })
        .await
        .unwrap();
    assert!(scoped.auth.to_auth_headers().unwrap().is_empty());
    assert!(scoped.agent_identity_telemetry.is_none());
    let (parent_result, keyed_result, local_result) = tokio::join!(
        send(&parent, "parent-model"),
        send(&keyed, "child-model"),
        send(&local, "local-model")
    );
    assert!(parent_result.is_ok());
    assert!(keyed_result.is_err());
    assert!(local_result.is_ok());
    assert_eq!(
        keyed.recover_from_unauthorized().await.unwrap(),
        ProviderUnauthorizedRecovery::NotConfigured
    );
    // Quota and transport retries stay on the selected endpoint too.
    keyed_server.reset().await;
    respond(&keyed_server, 429).await;
    assert!(send(&keyed, "child-model").await.is_err());
    assert!(send(&parent, "parent-after-child-error").await.is_ok());
    assert_eq!(host.auth().await, Some(host_auth()));
    for (server, expected_model, expected_token) in [
        (
            &parent_server,
            "parent-model",
            Some("Bearer synthetic-host-token"),
        ),
        (
            &keyed_server,
            "child-model",
            Some("Bearer synthetic-child-token"),
        ),
        (&local_server, "local-model", None),
    ] {
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0].body_json::<serde_json::Value>().unwrap()["model"],
            expected_model
        );
        for request in requests {
            assert_eq!(
                request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                expected_token
            );
            if !std::ptr::eq(server, &parent_server) {
                for header in [
                    "chatgpt-account-id",
                    "x-openai-actor-id",
                    "x-openai-fedramp",
                    crate::ACCOUNT_ROUTING_HEADER,
                ] {
                    assert!(!request.headers.contains_key(header));
                }
            }
        }
    }
}

#[tokio::test]
async fn unauthenticated_provider_gateway_uses_only_explicit_gateway_policy() {
    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    let host = AuthManager::from_auth_for_testing_with_home(host_auth(), home.path().to_path_buf());
    let mut info = independent_provider(&server);
    info.gateway_oauth = Some(GatewayOAuthConfig {
        authorization_url: format!("{}/authorize", server.uri()),
        token_url: format!("{}/token", server.uri()),
        client_id: "gateway-client".into(),
        resource: None,
        scopes: vec![],
        redirect_port: None,
        delivery: GatewayOAuthDelivery::Header {
            name: "x-gateway-auth".into(),
            scheme: "Bearer".into(),
        },
    });
    let gateway = crate::test_support::seed_gateway_auth(
        &info,
        &host,
        json!({"access_token": "synthetic-gateway-token", "refresh_token": "synthetic-refresh", "expires_at": i64::MAX}),
    );
    let provider = create_model_provider(info, Some(host));
    assert!(provider.auth_manager().is_none());
    assert!(Arc::ptr_eq(
        &provider.gateway_auth_manager().unwrap().unwrap(),
        &gateway
    ));
    let auth = provider.api_auth().await.unwrap();
    for headers in [
        auth.to_auth_headers().unwrap(),
        auth.resolve_auth_headers().await.unwrap(),
    ] {
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-gateway-auth"], "Bearer synthetic-gateway-token");
    }
}

#[tokio::test]
async fn env_key_overrides_host_auth_without_sharing_its_lifecycle() {
    const TEST: &str =
        "provider::child_auth_tests::env_key_overrides_host_auth_without_sharing_its_lifecycle";
    const KEY: &str = "CODEX_CHILD_PROVIDER_SYNTHETIC_ENV_KEY";
    // Set environment only in a subprocess; do not race other tests' environment.
    if std::env::var_os(KEY).is_none() {
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(KEY, "synthetic-env-child-token")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        return;
    }
    let server = MockServer::start().await;
    respond(&server, 200).await;
    let host = AuthManager::from_auth_for_testing(host_auth());
    let mut info = independent_provider(&server);
    info.env_key = Some(KEY.into());
    info.requires_openai_auth = true;
    let provider = create_model_provider(info, Some(host.clone()));
    assert!(provider.auth_manager().is_none());
    assert!(send(&provider, "env-model").await.is_ok());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer synthetic-env-child-token"
    );
    assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
    assert_eq!(host.auth().await, Some(host_auth()));
}

#[tokio::test]
async fn command_managers_are_independent_from_host_and_each_other() {
    let server = MockServer::start().await;
    respond(&server, 200).await;
    let home = tempfile::tempdir().unwrap();
    let token_file = home.path().join("token");
    std::fs::write(&token_file, "synthetic-command-token").unwrap();
    #[cfg(unix)]
    let (command, args) = ("cat", vec![token_file.to_string_lossy().into_owned()]);
    #[cfg(windows)]
    let (command, args) = (
        "cmd.exe",
        vec![
            "/D".into(),
            "/C".into(),
            "type".into(),
            token_file.to_string_lossy().into_owned(),
        ],
    );
    let mut info = independent_provider(&server);
    info.auth = Some(
        serde_json::from_value(json!({
            "command": command, "args": args, "cwd": home.path(),
            "timeout_ms": 5_000, "refresh_interval_ms": 0,
        }))
        .unwrap(),
    );
    let host = AuthManager::from_auth_for_testing(host_auth());
    let first = create_model_provider(info.clone(), Some(host.clone()));
    let second = create_model_provider(info, Some(host.clone()));
    let first_manager = first.auth_manager().unwrap();
    let second_manager = second.auth_manager().unwrap();
    assert!(!Arc::ptr_eq(&first_manager, &host));
    assert!(!Arc::ptr_eq(&first_manager, &second_manager));
    let (first_auth, second_auth) = tokio::join!(first.auth(), second.auth());
    assert_eq!(
        first_auth,
        Some(CodexAuth::from_api_key("synthetic-command-token"))
    );
    assert_eq!(second_auth, first_auth);
    assert!(send(&first, "first-command-model").await.is_ok());
    std::fs::write(&token_file, "synthetic-command-replacement").unwrap();
    let mut recovery = first_manager.unauthorized_recovery();
    assert!(recovery.has_next());
    recovery.next().await.unwrap();
    assert_eq!(
        first.auth().await,
        Some(CodexAuth::from_api_key("synthetic-command-replacement"))
    );
    assert_eq!(second.auth().await, second_auth);
    assert_eq!(host.auth().await, Some(host_auth()));
    let (first_result, second_result) = tokio::join!(
        send(&first, "first-command-model"),
        send(&second, "second-command-model")
    );
    assert!(first_result.is_ok());
    assert!(second_result.is_ok());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer synthetic-command-token"
    );
    for request in &requests[1..] {
        let body = request.body_json::<serde_json::Value>().unwrap();
        let expected = if body["model"] == "first-command-model" {
            "Bearer synthetic-command-replacement"
        } else {
            assert_eq!(body["model"], "second-command-model");
            "Bearer synthetic-command-token"
        };
        assert_eq!(request.headers["authorization"], expected);
        assert!(!request.headers.contains_key("chatgpt-account-id"));
    }
    let (first_headers, second_headers) = tokio::join!(first.api_auth(), second.api_auth());
    assert_eq!(first_headers.unwrap().to_auth_headers().unwrap().len(), 1);
    assert_eq!(second_headers.unwrap().to_auth_headers().unwrap().len(), 1);
}

#[tokio::test]
async fn connection_retry_keeps_the_selected_endpoint_and_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    respond(&server, 200).await;
    let host = AuthManager::from_auth_for_testing(host_auth());
    let mut info = independent_provider(&server);
    info.request_max_retries = Some(2);
    info.experimental_bearer_token = Some("synthetic-retry-token".into());
    let provider = create_model_provider(info, Some(host.clone()));
    assert!(send(&provider, "retry-model").await.is_ok());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.headers["authorization"],
            "Bearer synthetic-retry-token"
        );
        assert!(!request.headers.contains_key("chatgpt-account-id"));
        assert_eq!(
            request.body_json::<serde_json::Value>().unwrap()["model"],
            "retry-model"
        );
    }
    assert_eq!(host.auth().await, Some(host_auth()));
}
