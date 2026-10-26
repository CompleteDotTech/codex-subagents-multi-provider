//! Coverage for cache identity validation and conditional freshness updates.

use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[derive(Debug)]
struct ObservedEndpoint {
    persisted: Mutex<Option<String>>,
    observed: Mutex<Option<String>>,
    replace_during_fetch: bool,
}

impl ModelsEndpointClient for ObservedEndpoint {
    fn identity(&self) -> Option<String> {
        self.observed.lock().unwrap().clone()
    }
    fn reconcile_identity(&self) -> ModelsEndpointFuture<'_, ()> {
        Box::pin(async move {
            *self.observed.lock().unwrap() = self.persisted.lock().unwrap().clone();
        })
    }
    fn requires_identity_reconciliation(&self) -> bool {
        true
    }
    fn has_command_auth(&self) -> bool {
        true
    }
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }
    fn list_models<'a>(
        &'a self,
        _version: &'a str,
        _http: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, Result<ModelsEndpointResponse, ModelsEndpointError>> {
        Box::pin(async move {
            let identity = self.identity().unwrap();
            if self.replace_during_fetch {
                *self.persisted.lock().unwrap() = Some("grant-b".into());
            }
            Ok(ModelsEndpointResponse {
                models: vec![remote_model("shared", "Grant A", 0)],
                etag: None,
                identity,
            })
        })
    }
}

#[derive(Debug)]
struct ReplacingCache {
    endpoint: Arc<ObservedEndpoint>,
    replace_during_store: bool,
    entry: Mutex<Option<ModelsCacheEntry>>,
}

impl ModelsCache for ReplacingCache {
    fn load<'a>(
        &'a self,
        version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<Option<ModelsCacheEntry>, ModelsCacheError>> {
        Box::pin(async move {
            Ok(self
                .entry
                .lock()
                .unwrap()
                .clone()
                .filter(|entry| entry.client_version.as_deref() == Some(version)))
        })
    }
    fn store<'a>(
        &'a self,
        entry: &'a ModelsCacheEntry,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        Box::pin(async move {
            *self.entry.lock().unwrap() = Some(entry.clone());
            if self.replace_during_store {
                *self.endpoint.persisted.lock().unwrap() = Some("grant-b".into());
            }
            Ok(())
        })
    }
    fn refresh_ttl<'a>(
        &'a self,
        _version: &'a str,
        _identity: &'a str,
        _etag: &'a str,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn persisted_identity_changes_during_fetch_or_cache_write_cannot_publish_stale_metadata() {
    for during_fetch in [false, true] {
        let endpoint = Arc::new(ObservedEndpoint {
            persisted: Mutex::new(Some("grant-a".into())),
            observed: Mutex::new(None),
            replace_during_fetch: during_fetch,
        });
        let cache = Arc::new(ReplacingCache {
            endpoint: endpoint.clone(),
            replace_during_store: !during_fetch,
            entry: Mutex::new(None),
        });
        let manager = OpenAiModelsManager::new_with_cache(cache.clone(), endpoint.clone(), None)
            .with_provider_catalog();
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models
                .is_empty()
        );
        assert_eq!(endpoint.identity().as_deref(), Some("grant-b"));
        assert_eq!(cache.entry.lock().unwrap().is_some(), !during_fetch);
        assert!(manager.get_remote_models().await.is_empty());
        assert!(manager.try_get_remote_models().unwrap().is_empty());
        assert!(
            manager
                .get_model_info("shared", &ModelsManagerConfig::default())
                .await
                .used_fallback_model_metadata
        );
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models
                .is_empty()
        );
    }
}

#[tokio::test]
async fn metadata_reads_observe_persisted_identity_changes_without_catalog_refresh() {
    let endpoint = Arc::new(ObservedEndpoint {
        persisted: Mutex::new(Some("grant-a".into())),
        observed: Mutex::new(None),
        replace_during_fetch: false,
    });
    let manager =
        OpenAiModelsManager::new_without_cache(endpoint.clone(), None).with_provider_catalog();
    assert_eq!(
        manager
            .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
            .await
            .models
            .len(),
        1
    );
    assert!(
        !manager
            .get_model_info("shared", &ModelsManagerConfig::default())
            .await
            .used_fallback_model_metadata
    );
    *endpoint.persisted.lock().unwrap() = None;
    assert!(
        manager
            .get_model_info("shared", &ModelsManagerConfig::default())
            .await
            .used_fallback_model_metadata
    );
    assert!(manager.get_remote_models().await.is_empty());
    assert!(manager.try_get_remote_models().unwrap().is_empty());
}

#[tokio::test]
async fn partitioned_files_preserve_same_slug_metadata_for_each_provider_and_tenant() {
    let home = tempdir().unwrap();
    let path = home.path().join(MODEL_CACHE_FILE);
    let cache = FileModelsCache::partitioned(path.clone(), DEFAULT_MODEL_CACHE_TTL);
    let version = crate::client_version_to_whole();
    let mut first = remote_model("shared", "OpenAI", 0);
    first.context_window = Some(128_000);
    let mut second = remote_model("shared", "GLM", 0);
    second.context_window = Some(32_000);
    second.default_reasoning_level = Some(ReasoningEffort::Low);
    let entry = |identity: &str, model| ModelsCacheEntry {
        fetched_at: Utc::now(),
        etag: Some("same-etag".into()),
        client_version: Some(version.clone()),
        identity: Some(identity.into()),
        models: vec![model],
    };
    let first = entry("provider-a/account-1", first);
    let second = entry("provider-b/account-2", second);
    let (a, b) = tokio::join!(cache.store(&first), cache.store(&second));
    a.unwrap();
    b.unwrap();
    // A generic cache file must not supply metadata for an unknown identity.
    FileModelsCache::new(path, DEFAULT_MODEL_CACHE_TTL)
        .store(&first)
        .await
        .unwrap();
    assert!(
        cache
            .load_scoped(&version, "other-tenant")
            .await
            .unwrap()
            .is_none()
    );
    for _ in 0..3 {
        for expected in [&first, &second] {
            assert_eq!(
                cache
                    .load_scoped(&version, expected.identity.as_deref().unwrap())
                    .await
                    .unwrap(),
                Some(expected.clone())
            );
        }
    }
    let reopened =
        FileModelsCache::partitioned(home.path().join(MODEL_CACHE_FILE), DEFAULT_MODEL_CACHE_TTL);
    assert_eq!(
        reopened
            .load_scoped(&version, "provider-a/account-1")
            .await
            .unwrap(),
        Some(first)
    );
    assert_eq!(
        reopened
            .load_scoped(&version, "provider-b/account-2")
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn mismatched_and_legacy_cache_entries_fetch_the_current_catalog() {
    for identity in [None, Some("other-provider-or-account".to_string())] {
        let stale = remote_model("stale", "Stale", /*priority*/ 0);
        let current = remote_model("current", "Current", /*priority*/ 0);
        let cache = TestModelsCache::with_entry(ModelsCacheEntry {
            fetched_at: Utc::now(),
            etag: Some("old-etag".into()),
            client_version: Some(crate::client_version_to_whole()),
            identity,
            models: vec![stale],
        });
        let endpoint = TestModelsEndpoint::new(vec![vec![current.clone()]]);
        let manager = OpenAiModelsManager::new_with_cache(
            cache.clone(),
            endpoint.clone(),
            Some(AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            )),
        );
        assert_eq!(
            manager
                .raw_model_catalog(
                    RefreshStrategy::OnlineIfUncached,
                    DEFAULT_HTTP_CLIENT_FACTORY
                )
                .await
                .models,
            vec![current.clone()]
        );
        assert_eq!(endpoint.fetch_count(), 1);
    }
}

#[tokio::test]
async fn matching_etag_cannot_renew_a_different_identity_or_catalog() {
    let home = tempdir().unwrap();
    let cache = FileModelsCache::new(home.path().join(MODEL_CACHE_FILE), DEFAULT_MODEL_CACHE_TTL);
    let expected = ModelsCacheEntry {
        fetched_at: Utc::now() - chrono::Duration::hours(1),
        etag: Some("etag".into()),
        client_version: Some(crate::client_version_to_whole()),
        identity: Some("first-account".into()),
        models: vec![remote_model("model", "Model", /*priority*/ 0)],
    };
    for (key, etag, version) in [
        ("other-account", "etag", crate::client_version_to_whole()),
        (
            "first-account",
            "new-etag",
            crate::client_version_to_whole(),
        ),
        ("first-account", "etag", "other-version".into()),
    ] {
        let stored = ModelsCacheEntry {
            identity: Some(key.into()),
            etag: Some(etag.into()),
            client_version: Some(version),
            ..expected.clone()
        };
        cache.store(&stored).await.unwrap();
        cache
            .refresh_ttl(
                expected.client_version.as_deref().unwrap(),
                expected.identity.as_deref().unwrap(),
                expected.etag.as_deref().unwrap(),
            )
            .await
            .unwrap();
        let actual: ModelsCacheEntry = serde_json::from_slice(
            &tokio::fs::read(home.path().join(MODEL_CACHE_FILE))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, stored);
    }
}

#[derive(Debug)]
struct RejectedAttemptEndpoint {
    observed: Mutex<String>,
    reject: std::sync::atomic::AtomicBool,
}
impl ModelsEndpointClient for RejectedAttemptEndpoint {
    fn identity(&self) -> Option<String> {
        Some(self.observed.lock().unwrap().clone())
    }
    fn has_command_auth(&self) -> bool {
        true
    }
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }
    fn list_models<'a>(
        &'a self,
        _: &'a str,
        _: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, Result<ModelsEndpointResponse, ModelsEndpointError>> {
        Box::pin(async move {
            if self.reject.swap(false, Ordering::SeqCst) {
                // The actual rejected B request differs from preflight/current A.
                return Err(ModelsEndpointError {
                    error: catalog_http_error(403),
                    identity: Some("grant-b".into()),
                });
            }
            Ok(ModelsEndpointResponse {
                identity: self.identity().unwrap(),
                etag: None,
                models: vec![remote_model("shared", "fresh", 0)],
            })
        })
    }
}

#[tokio::test]
async fn rejection_survives_account_switches_when_persistent_invalidation_fails() {
    for load_error in [false, true] {
        let stale = vec![remote_model("shared", "stale B", 0)];
        let cache = Arc::new(TestModelsCache {
            entry: Mutex::new(Some(ModelsCacheEntry {
                fetched_at: Utc::now(),
                etag: None,
                client_version: Some(crate::client_version_to_whole()),
                identity: Some("grant-b".into()),
                models: stale.clone(),
            })),
            load_error,
            store_error: true,
            stored_entries: Mutex::new(Vec::new()),
        });
        let endpoint = Arc::new(RejectedAttemptEndpoint {
            observed: Mutex::new("grant-a".into()),
            reject: std::sync::atomic::AtomicBool::new(true),
        });
        let manager = OpenAiModelsManager::new_with_cache(cache.clone(), endpoint.clone(), None)
            .with_provider_catalog();
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models
                .is_empty()
        );
        // Unrelated A success must not erase the B rejection marker.
        assert_eq!(
            manager
                .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models[0]
                .display_name,
            "fresh"
        );
        *endpoint.observed.lock().unwrap() = "grant-b".into();
        assert!(
            manager
                .raw_model_catalog(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
                .await
                .models
                .is_empty()
        );
        assert!(manager.get_remote_models().await.is_empty());
        assert!(
            manager
                .get_model_info("shared", &Default::default())
                .await
                .used_fallback_model_metadata
        );
        assert_eq!(cache.entry.lock().unwrap().as_ref().unwrap().models, stale);
        // Only an authenticated B fetch rehabilitates B, even when cache writes still fail.
        assert_eq!(
            manager
                .raw_model_catalog(
                    RefreshStrategy::OnlineIfUncached,
                    DEFAULT_HTTP_CLIENT_FACTORY
                )
                .await
                .models[0]
                .display_name,
            "fresh"
        );
        assert!(!manager.identity_rejected("grant-b"));
    }
}

#[tokio::test]
async fn pinned_authority_snapshot_is_nonblocking_and_never_reconciles_or_fetches() {
    let endpoint = Arc::new(ObservedEndpoint {
        persisted: Mutex::new(Some("grant-b".into())),
        observed: Mutex::new(Some("grant-a".into())),
        replace_during_fetch: false,
    });
    let manager =
        OpenAiModelsManager::new_without_cache(endpoint.clone(), None).with_provider_catalog();
    manager.remote_models.write().await.models = vec![remote_model("shared", "Pinned grant A", 0)];
    let config = ModelsManagerConfig::default();
    let info = manager
        .try_pinned_authority_model("shared", &config)
        .expect("validated pinned catalog");
    assert_eq!(info.display_name, "Pinned grant A");
    assert_eq!(
        endpoint.identity().as_deref(),
        Some("grant-a"),
        "snapshot must not execute credential reconciliation"
    );
    assert!(
        manager
            .try_pinned_authority_model("missing", &config)
            .is_none()
    );
    let held = manager.remote_models.write().await;
    assert!(
        manager
            .try_pinned_authority_model("shared", &config)
            .is_none(),
        "busy lock must not block"
    );
    drop(held);
    *endpoint.observed.lock().unwrap() = Some("grant-b".into());
    assert!(
        manager
            .try_pinned_authority_model("shared", &config)
            .is_none(),
        "known cached identity mismatch"
    );
    *endpoint.observed.lock().unwrap() = Some("grant-a".into());
    manager.remote_models.write().await.fetched_at = chrono::DateTime::<Utc>::MIN_UTC;
    assert!(
        manager
            .try_pinned_authority_model("shared", &config)
            .is_none(),
        "invalidated metadata is not proof"
    );
}

#[test]
fn pinned_static_authority_snapshot_uses_only_immutable_available_metadata() {
    let manager = StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![remote_model("static", "Static", 0)],
        },
    );
    assert_eq!(
        manager
            .try_pinned_authority_model("static", &Default::default())
            .unwrap()
            .slug,
        "static"
    );
    assert!(
        manager
            .try_pinned_authority_model("missing", &Default::default())
            .is_none()
    );
}
