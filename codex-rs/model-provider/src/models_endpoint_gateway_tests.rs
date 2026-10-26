use super::*;
use crate::test_support::seed_gateway_auth;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::GatewayOAuthConfig;
use codex_model_provider_info::GatewayOAuthDelivery;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::openai_models::ModelInfo;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const HTTP: HttpClientFactory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);

fn info(server: &MockServer) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "Gateway catalog fixture".into(),
        requires_openai_auth: false,
        model_catalog_url: Some(format!("{}/models", server.uri()).into()),
        gateway_oauth: Some(GatewayOAuthConfig {
            authorization_url: format!("{}/authorize", server.uri()),
            token_url: format!("{}/token", server.uri()),
            client_id: "client".into(),
            resource: None,
            scopes: vec![],
            redirect_port: None,
            delivery: GatewayOAuthDelivery::Header {
                name: "x-gateway-auth".into(),
                scheme: "Bearer".into(),
            },
        }),
        ..ModelProviderInfo::create_openai_provider(Some(server.uri()))
    }
}

fn model(window: i64) -> ModelInfo {
    let mut model = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .remove(0);
    model.slug = "shared".into();
    model.context_window = Some(window);
    model.max_context_window = Some(window);
    model
}

async fn mount(server: &MockServer, token: &str, window: i64, delay: Duration) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("x-gateway-auth", format!("Bearer {token}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({"models": [model(window)]})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn gateway_only_catalog_scope_observes_external_accounts_legacy_grants_and_refresh() {
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let primary = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("never-forward-primary"),
        home.path().to_path_buf(),
    );
    let info = info(&server);
    let gateway = seed_gateway_auth(
        &info,
        &primary,
        json!({"access_token": "legacy", "expires_at": i64::MAX}),
    );
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(
        info.clone(),
        None,
        Some(Ok(gateway.clone())),
    ));
    let manager = OpenAiModelsManager::new(home.path().to_path_buf(), endpoint.clone(), None)
        .with_provider_catalog();
    mount(&server, "legacy", 1000, Duration::ZERO).await;
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
            .await
            .models[0]
            .context_window,
        Some(1000)
    );
    let legacy_scope = endpoint.identity().unwrap();
    let _replacement = seed_gateway_auth(
        &info,
        &primary,
        json!({"access_token": "account-b", "authorization_id": "grant-b", "refresh_token": "refresh-b", "expires_at": i64::MAX}),
    );
    // A metadata lookup must observe disk replacement even without a refresh/list operation.
    assert!(
        manager
            .get_model_info("shared", &Default::default())
            .await
            .used_fallback_model_metadata
    );
    assert_ne!(endpoint.identity().unwrap(), legacy_scope);
    assert!(manager.try_get_remote_models().unwrap().is_empty());
    mount(&server, "account-b", 2000, Duration::ZERO).await;
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
            .await
            .models[0]
            .context_window,
        Some(2000)
    );
    let account_b_scope = endpoint.identity().unwrap();
    let _expired = seed_gateway_auth(
        &info,
        &primary,
        json!({"access_token": "account-b", "authorization_id": "grant-b", "refresh_token": "refresh-b", "expires_at": 0}),
    );
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "rotated-b", "token_type": "Bearer", "expires_in": 3600}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    mount(&server, "rotated-b", 3000, Duration::ZERO).await;
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
            .await
            .models[0]
            .context_window,
        Some(3000)
    );
    assert_eq!(gateway.authorization_id().await.unwrap(), "grant-b");
    assert_ne!(endpoint.identity().unwrap(), account_b_scope);
    for request in server.received_requests().await.unwrap() {
        assert!(!request.headers.contains_key("authorization"));
    }
    gateway.logout().await.unwrap();
    assert!(manager.get_remote_models().await.is_empty());
    assert!(endpoint.identity().is_none());
    assert!(
        manager
            .raw_model_catalog(RefreshStrategy::Offline, HTTP)
            .await
            .models
            .is_empty()
    );
}

#[tokio::test]
async fn changed_gateway_grant_during_catalog_response_cannot_publish_or_cache_old_metadata() {
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let primary = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("unused"),
        home.path().to_path_buf(),
    );
    let info = info(&server);
    let gateway = seed_gateway_auth(
        &info,
        &primary,
        json!({"access_token": "old", "authorization_id": "old", "expires_at": i64::MAX}),
    );
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(
        info.clone(),
        None,
        Some(Ok(gateway)),
    ));
    let manager = Arc::new(
        OpenAiModelsManager::new(home.path().to_path_buf(), endpoint, None).with_provider_catalog(),
    );
    mount(&server, "old", 1111, Duration::from_millis(300)).await;
    let pending = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .raw_model_catalog(RefreshStrategy::Online, HTTP)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let _replacement = seed_gateway_auth(
        &info,
        &primary,
        json!({"access_token": "new", "authorization_id": "new", "expires_at": i64::MAX}),
    );
    assert!(pending.await.unwrap().models.is_empty());
    assert!(manager.get_remote_models().await.is_empty());
    assert!(!home.path().join("models_cache.d").exists());
    mount(&server, "new", 2222, Duration::ZERO).await;
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
            .await
            .models[0]
            .context_window,
        Some(2222)
    );
}

#[tokio::test]
async fn trusted_unauthenticated_catalog_url_is_discovered_through_the_models_manager() {
    let server = MockServer::start().await;
    let mut info = info(&server);
    info.gateway_oauth = None;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [model(1234)]})))
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(info, None, None));
    let manager = OpenAiModelsManager::new_without_cache(endpoint, None).with_provider_catalog();
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
            .await
            .models[0]
            .context_window,
        Some(1234)
    );
    assert!(
        server.received_requests().await.unwrap()[0]
            .headers
            .get("authorization")
            .is_none()
    );
}
