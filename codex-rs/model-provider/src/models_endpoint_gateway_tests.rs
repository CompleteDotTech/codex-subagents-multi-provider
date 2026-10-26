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

#[tokio::test]
async fn catalog_metadata_fails_closed_promptly_when_the_real_store_lock_is_busy() {
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
        json!({"access_token":"lock-test", "expires_at":i64::MAX}),
    );
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(info, None, Some(Ok(gateway))));
    let manager =
        OpenAiModelsManager::new(home.path().to_path_buf(), endpoint, None).with_provider_catalog();
    mount(&server, "lock-test", 1234, Duration::ZERO).await;
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, HTTP)
            .await
            .models
            .len(),
        1
    );
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.path().join("secrets/gateway_oauth.lock"))
        .unwrap();
    guard.try_lock().unwrap();
    tokio::time::timeout(Duration::from_millis(250), async {
        assert!(
            manager
                .get_model_info("shared", &Default::default())
                .await
                .used_fallback_model_metadata
        );
        assert!(manager.get_remote_models().await.is_empty());
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, HTTP)
                .await
                .models
                .is_empty()
        );
    })
    .await
    .expect("metadata observation must not wait for token exchange");
    drop(guard);
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Offline, HTTP)
            .await
            .models[0]
            .context_window,
        Some(1234)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// Pause before the real endpoint resolves request credentials, after manager preflight.
#[derive(Debug)]
struct PausedGatewayEndpoint {
    inner: Arc<OpenAiModelsEndpoint>,
    pause: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl ModelsEndpointClient for PausedGatewayEndpoint {
    fn identity(&self) -> Option<String> {
        self.inner.identity()
    }
    fn reconcile_identity(&self) -> ModelsEndpointFuture<'_, ()> {
        self.inner.reconcile_identity()
    }
    fn requires_identity_reconciliation(&self) -> bool {
        true
    }
    fn has_command_auth(&self) -> bool {
        self.inner.has_command_auth()
    }
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(self.inner.uses_codex_backend())
    }
    fn list_models<'a>(
        &'a self,
        version: &'a str,
        http: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, Result<ModelsEndpointResponse, ModelsEndpointError>> {
        Box::pin(async move {
            if self.pause.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.list_models(version, http).await
        })
    }
}

#[tokio::test]
async fn rejected_catalog_invalidates_the_sent_grant_after_a_preflight_account_switch() {
    for (status, concurrent_reader) in [(401, false), (403, true)] {
        let home = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let primary = AuthManager::from_auth_for_testing_with_home(
            CodexAuth::from_api_key("never-forward-primary"),
            home.path().to_path_buf(),
        );
        let info = info(&server);
        let grant = |token: &str| json!({"access_token": token,"authorization_id": token,"expires_at": i64::MAX});
        let gateway = seed_gateway_auth(&info, &primary, grant("account-a"));
        let endpoint = Arc::new(PausedGatewayEndpoint {
            inner: Arc::new(OpenAiModelsEndpoint::new(
                info.clone(),
                None,
                Some(Ok(gateway)),
            )),
            pause: std::sync::atomic::AtomicBool::new(false),
            entered: Default::default(),
            release: Default::default(),
        });
        let manager = Arc::new(
            OpenAiModelsManager::new(home.path().to_path_buf(), endpoint.clone(), None)
                .with_provider_catalog(),
        );
        mount(&server, "account-a", 1000, Duration::ZERO).await;
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, HTTP)
                .await
                .models[0]
                .context_window,
            Some(1000)
        );
        let _b = seed_gateway_auth(&info, &primary, grant("account-b"));
        mount(&server, "account-b", 2000, Duration::ZERO).await;
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, HTTP)
                .await
                .models[0]
                .context_window,
            Some(2000)
        );
        let _a = seed_gateway_auth(&info, &primary, grant("account-a"));
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, HTTP)
                .await
                .models[0]
                .context_window,
            Some(1000)
        );
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("x-gateway-auth", "Bearer account-b"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        endpoint
            .pause
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pending = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .raw_model_catalog(RefreshStrategy::Online, HTTP)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), endpoint.entered.notified())
            .await
            .unwrap();
        let _b = seed_gateway_auth(&info, &primary, grant("account-b"));
        if concurrent_reader {
            let _ = manager.get_remote_models().await;
        }
        endpoint.release.notify_one();
        assert!(pending.await.unwrap().models.is_empty());
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, HTTP)
                .await
                .models
                .is_empty()
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers["x-gateway-auth"], "Bearer account-b");
        assert!(!requests[0].headers.contains_key("authorization"));
        let _a = seed_gateway_auth(&info, &primary, grant("account-a"));
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, HTTP)
                .await
                .models[0]
                .context_window,
            Some(1000)
        );
        let _b = seed_gateway_auth(&info, &primary, grant("account-b"));
        server.verify().await;
        server.reset().await;
        mount(&server, "account-b", 3000, Duration::ZERO).await;
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::OnlineIfUncached, HTTP)
                .await
                .models[0]
                .context_window,
            Some(3000)
        );
    }
}

#[tokio::test]
async fn pinned_identity_snapshot_uses_only_cached_gateway_state_without_http_or_reconciliation() {
    let server = MockServer::start().await;
    let endpoint = OpenAiModelsEndpoint::new(
        info(&server),
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "synthetic",
        ))),
        None,
    );
    assert!(
        endpoint.try_pinned_identity().is_none(),
        "unobserved gateway scope is unavailable"
    );
    *endpoint.gateway_scope.write().unwrap() = Some("already-validated-scope".into());
    assert!(endpoint.try_pinned_identity().is_some());
    let reconciliation = endpoint.gateway_reconciliation.try_lock().unwrap();
    assert!(
        endpoint.try_pinned_identity().is_none(),
        "active reconciliation cannot block capture"
    );
    drop(reconciliation);
    let scope_writer = endpoint.gateway_scope.write().unwrap();
    assert!(
        endpoint.try_pinned_identity().is_none(),
        "busy cached scope cannot block capture"
    );
    drop(scope_writer);
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("request recording enabled")
            .len(),
        0
    );
}
