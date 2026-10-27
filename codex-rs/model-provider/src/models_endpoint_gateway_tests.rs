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
        /*auth_manager*/ None,
        Some(Ok(gateway.clone())),
    ));
    let manager = OpenAiModelsManager::new(
        home.path().to_path_buf(),
        endpoint.clone(),
        /*auth_manager*/ None,
    )
    .with_provider_catalog();
    mount(&server, "legacy", /*window*/ 1000, Duration::ZERO).await;
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
    mount(&server, "account-b", /*window*/ 2000, Duration::ZERO).await;
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
    mount(&server, "rotated-b", /*window*/ 3000, Duration::ZERO).await;
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
        /*auth_manager*/ None,
        Some(Ok(gateway)),
    ));
    let manager = Arc::new(
        OpenAiModelsManager::new(
            home.path().to_path_buf(),
            endpoint,
            /*auth_manager*/ None,
        )
        .with_provider_catalog(),
    );
    mount(
        &server,
        "old",
        /*window*/ 1111,
        Duration::from_millis(300),
    )
    .await;
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
    mount(&server, "new", /*window*/ 2222, Duration::ZERO).await;
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
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models": [model(/*window*/ 1234)]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(
        info, /*auth_manager*/ None, /*gateway_auth_manager*/ None,
    ));
    let manager = OpenAiModelsManager::new_without_cache(endpoint, /*auth_manager*/ None)
        .with_provider_catalog();
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
    let endpoint = Arc::new(OpenAiModelsEndpoint::new(
        info,
        /*auth_manager*/ None,
        Some(Ok(gateway)),
    ));
    let manager = OpenAiModelsManager::new(
        home.path().to_path_buf(),
        endpoint,
        /*auth_manager*/ None,
    )
    .with_provider_catalog();
    mount(&server, "lock-test", /*window*/ 1234, Duration::ZERO).await;
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
                /*auth_manager*/ None,
                Some(Ok(gateway)),
            )),
            pause: std::sync::atomic::AtomicBool::new(false),
            entered: Default::default(),
            release: Default::default(),
        });
        let manager = Arc::new(
            OpenAiModelsManager::new(
                home.path().to_path_buf(),
                endpoint.clone(),
                /*auth_manager*/ None,
            )
            .with_provider_catalog(),
        );
        mount(&server, "account-a", /*window*/ 1000, Duration::ZERO).await;
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, HTTP)
                .await
                .models[0]
                .context_window,
            Some(1000)
        );
        let _b = seed_gateway_auth(&info, &primary, grant("account-b"));
        mount(&server, "account-b", /*window*/ 2000, Duration::ZERO).await;
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
        mount(&server, "account-b", /*window*/ 3000, Duration::ZERO).await;
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
        /*gateway_auth_manager*/ None,
    );
    assert!(
        endpoint.try_pinned_identity().is_none(),
        "unobserved gateway scope is unavailable"
    );
    *endpoint.gateway_scope.write().unwrap() = Some("already-validated-scope".into());
    assert!(endpoint.try_pinned_identity().is_some());
    let reconciliation = endpoint
        .gateway_reconciliation
        .try_acquire()
        .expect("single reconciliation permit");
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

#[derive(Debug)]
struct GatewayReadGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
    finished: tokio::sync::oneshot::Sender<()>,
}

struct PausedGatewayRead {
    entered: tokio::sync::oneshot::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    finished: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Debug)]
struct BlockingGatewayKeyring {
    async_thread: std::thread::ThreadId,
    gate: std::sync::Mutex<Option<GatewayReadGate>>,
}

impl BlockingGatewayKeyring {
    fn pause_next_read(&self) -> PausedGatewayRead {
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished) = tokio::sync::oneshot::channel();
        let mut gate = self.gate.lock().expect("gateway read gate lock");
        assert!(gate.is_none());
        *gate = Some(GatewayReadGate {
            entered: entered_tx,
            release: release_rx,
            finished: finished_tx,
        });
        PausedGatewayRead {
            entered,
            release,
            finished,
        }
    }
}

impl codex_keyring_store::KeyringStore for BlockingGatewayKeyring {
    fn load(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<String>, codex_keyring_store::CredentialStoreError> {
        assert_ne!(std::thread::current().id(), self.async_thread);
        let gate = self.gate.lock().expect("gateway read gate lock").take();
        if let Some(gate) = gate {
            gate.entered
                .send(())
                .expect("gateway read entered receiver");
            gate.release
                .recv_timeout(Duration::from_secs(/*secs*/ 10))
                .expect("release actual gateway credential read");
            gate.finished
                .send(())
                .expect("gateway read finished receiver");
        }
        // The existing encrypted-store fixture uses this public test passphrase.
        Ok(Some("gateway-test-passphrase".to_owned()))
    }

    fn save(
        &self,
        _service: &str,
        _account: &str,
        _value: &str,
    ) -> Result<(), codex_keyring_store::CredentialStoreError> {
        panic!("catalog observation must not write keyring credentials")
    }

    fn delete(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<bool, codex_keyring_store::CredentialStoreError> {
        panic!("catalog observation must not delete keyring credentials")
    }
}

struct GatewayReconciliationFixture {
    home: tempfile::TempDir,
    server: MockServer,
    endpoint: Arc<OpenAiModelsEndpoint>,
    keyring: Arc<BlockingGatewayKeyring>,
}

impl GatewayReconciliationFixture {
    async fn new() -> Self {
        let home = tempfile::tempdir().expect("gateway reconciliation home");
        let server = MockServer::start().await;
        let primary = AuthManager::from_auth_for_testing_with_home(
            CodexAuth::from_api_key("synthetic-primary"),
            home.path().to_path_buf(),
        );
        let provider = info(&server);
        let _seed = seed_gateway_auth(
            &provider,
            &primary,
            json!({"access_token":"single-flight-test", "expires_at":i64::MAX}),
        );
        let config = provider
            .gateway_oauth
            .as_ref()
            .expect("gateway fixture config");
        let keyring = Arc::new(BlockingGatewayKeyring {
            async_thread: std::thread::current().id(),
            gate: std::sync::Mutex::new(/*t*/ None),
        });
        let gateway = GatewayAuthManager::new(
            codex_login::GatewayAuthConfig {
                authorization_url: config.authorization_url.clone(),
                device_authorization_url: None,
                token_url: config.token_url.clone(),
                client_id: config.client_id.clone(),
                resource: config.resource.clone(),
                scopes: config.scopes.clone(),
                redirect_port: config.redirect_port,
            },
            home.path().to_path_buf(),
            &HTTP,
            keyring.clone(),
        )
        .expect("real gateway observer")
        .without_interactive_login();
        let endpoint = Arc::new(OpenAiModelsEndpoint::new(
            provider,
            Some(primary),
            Some(Ok(Arc::new(gateway))),
        ));
        endpoint.reconcile_identity().await;
        assert!(endpoint.try_pinned_identity().is_some());
        Self {
            home,
            server,
            endpoint,
            keyring,
        }
    }

    async fn assert_no_http(&self) {
        assert!(
            self.server
                .received_requests()
                .await
                .expect("request recording enabled")
                .is_empty()
        );
    }
}

async fn reconciliation_is_pending(future: &mut ModelsEndpointFuture<'_, ()>) -> bool {
    std::future::poll_fn(|context| {
        std::task::Poll::Ready(future.as_mut().poll(context).is_pending())
    })
    .await
}

#[tokio::test]
async fn gateway_reconciliation_serializes_actual_observers_in_fifo_order() {
    let fixture = GatewayReconciliationFixture::new().await;
    let held = fixture
        .endpoint
        .gateway_reconciliation
        .try_acquire()
        .expect("initial admission permit");
    let mut first = fixture.endpoint.reconcile_identity();
    let mut second = fixture.endpoint.reconcile_identity();
    assert!(reconciliation_is_pending(&mut first).await);
    assert!(reconciliation_is_pending(&mut second).await);
    assert!(fixture.endpoint.try_pinned_identity().is_none());
    drop(held);
    assert!(
        reconciliation_is_pending(&mut second).await,
        "second observer cannot bypass the first queued observer"
    );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), first)
        .await
        .expect("first actual observation");
    assert!(
        fixture.endpoint.try_pinned_identity().is_none(),
        "queued second observer retains exclusive admission"
    );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), second)
        .await
        .expect("second actual observation");
    assert!(fixture.endpoint.try_pinned_identity().is_some());
    fixture.assert_no_http().await;
}

#[tokio::test]
async fn gateway_reconciliation_blocks_pinned_capture_during_a_real_credential_read() {
    let fixture = GatewayReconciliationFixture::new().await;
    let PausedGatewayRead {
        entered,
        release,
        finished,
    } = fixture.keyring.pause_next_read();
    let observer = Arc::clone(&fixture.endpoint);
    let task = tokio::spawn(async move { observer.reconcile_identity().await });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), entered)
        .await
        .expect("actual observer enters keyring")
        .expect("entered sender");
    assert!(
        fixture
            .endpoint
            .gateway_scope
            .read()
            .expect("scope lock")
            .is_none()
    );
    let snapshot = Arc::clone(&fixture.endpoint);
    let capture = tokio::time::timeout(
        Duration::from_millis(/*millis*/ 250),
        tokio::task::spawn_blocking(move || snapshot.try_pinned_identity()),
    )
    .await;
    release.send(()).expect("release observer");
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), finished)
        .await
        .expect("keyring read finishes")
        .expect("finished sender");
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), task)
        .await
        .expect("actual reconciliation finishes")
        .expect("observer task");
    assert!(
        capture
            .expect("pinned capture must remain nonblocking")
            .expect("capture task")
            .is_none()
    );
    assert!(fixture.endpoint.try_pinned_identity().is_some());
    fixture.assert_no_http().await;
}

#[tokio::test]
async fn gateway_reconciliation_cancellation_keeps_scope_unavailable_until_a_new_observation() {
    let fixture = GatewayReconciliationFixture::new().await;
    let PausedGatewayRead {
        entered,
        release,
        finished,
    } = fixture.keyring.pause_next_read();
    let observer = Arc::clone(&fixture.endpoint);
    let task = tokio::spawn(async move { observer.reconcile_identity().await });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), entered)
        .await
        .expect("actual observer enters keyring")
        .expect("entered sender");
    task.abort();
    let cancelled = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), task)
        .await
        .expect("cancelled reconciliation joins");
    assert!(matches!(cancelled, Err(error) if error.is_cancelled()));
    assert_eq!(
        fixture.endpoint.gateway_reconciliation.available_permits(),
        1
    );
    assert!(fixture.endpoint.try_pinned_identity().is_none());
    release.send(()).expect("release detached credential read");
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), finished)
        .await
        .expect("detached keyring read finishes")
        .expect("finished sender");
    // Observe release of the real credential-store lock, not an arbitrary delay.
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home.path().join("secrets/gateway_oauth.lock"))
        .expect("real gateway credential lock");
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        loop {
            if probe.try_lock().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached observer releases credential-store ownership");
    assert!(
        fixture.endpoint.try_pinned_identity().is_none(),
        "cancelled observer cannot publish its detached result"
    );
    drop(probe);
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        fixture.endpoint.reconcile_identity(),
    )
    .await
    .expect("fresh actual observation");
    assert!(fixture.endpoint.try_pinned_identity().is_some());
    fixture.assert_no_http().await;
}

#[tokio::test]
async fn closed_gateway_reconciliation_admission_fails_closed() {
    let fixture = GatewayReconciliationFixture::new().await;
    fixture.endpoint.gateway_reconciliation.close();
    assert!(fixture.endpoint.try_pinned_identity().is_none());
    fixture.endpoint.reconcile_identity().await;
    assert!(
        fixture
            .endpoint
            .gateway_scope
            .read()
            .expect("scope lock")
            .is_none()
    );
    assert!(fixture.endpoint.try_pinned_identity().is_none());
    fixture.assert_no_http().await;
}
