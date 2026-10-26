//! Coverage for cache identity validation and conditional freshness updates.

use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

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
