//! Request credentials retain their grant identity through unauthorized recovery.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn derived_cache_scope_observes_persisted_grants_and_actual_request_credentials() {
    let server = MockServer::start().await;
    let (manager, _home) = client(config(&server), Arc::new(MockKeyringStore::default()));
    let mut previous = None;
    for (access, grant) in [
        ("legacy", None),
        ("account-a", Some("a")),
        ("rotated-a", Some("a")),
        ("account-b", Some("b")),
    ] {
        manager
            .save_token(&StoredToken {
                access_token: access.into(),
                authorization_id: grant.map(str::to_owned),
                refresh_token: None,
                expires_at: Some(Utc::now().timestamp() + 3600),
            })
            .unwrap();
        let scope = manager
            .observe_credential_identity()
            .await
            .unwrap()
            .unwrap();
        let captured = manager.begin_request().credentials().await.unwrap();
        assert_eq!(scope, captured.cache_identity(&manager.credential_id()));
        assert_ne!(previous.as_ref(), Some(&scope));
        assert!(!format!("{captured:?}").contains(access));
        assert_eq!(scope.len(), 64);
        previous = Some(scope);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn derived_cache_scope_is_unknown_for_busy_pending_rejected_expired_or_missing_state() {
    let server = MockServer::start().await;
    let (manager, _home) = client(config(&server), Arc::new(MockKeyringStore::default()));
    let token = StoredToken {
        access_token: "secret".into(),
        authorization_id: Some("grant".into()),
        refresh_token: None,
        expires_at: Some(Utc::now().timestamp() + 3600),
    };
    assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
    manager.save_token(&token).unwrap();
    assert!(
        manager
            .observe_credential_identity()
            .await
            .unwrap()
            .is_some()
    );
    {
        let mut cached = manager.state.cached_token.lock().await;
        assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
        cached.token = Some(token.clone());
        cached.pending = Some(token.clone());
    }
    assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
    {
        let mut cached = manager.state.cached_token.lock().await;
        cached.pending = None;
        cached.login_required = Some(token.clone());
    }
    assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
    manager.state.cached_token.lock().await.login_required = None;
    manager.publish_status(
        super::super::OAuthSessionStatus::Started,
        /*credential*/ None,
    );
    assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
    manager.publish_status(super::super::OAuthSessionStatus::Succeeded, Some(&token));
    manager
        .save_token(&StoredToken {
            expires_at: Some(0),
            ..token
        })
        .unwrap();
    assert_eq!(manager.observe_credential_identity().await.unwrap(), None);
    manager
        .state
        .storage
        .save(&manager.credential_id(), "invalid credential encoding")
        .unwrap();
    assert!(manager.observe_credential_identity().await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn request_refresh_retry_keeps_the_same_sign_in() {
    let server = MockServer::start().await;
    let (manager, _home) = client(config(&server), Arc::new(MockKeyringStore::default()));
    manager
        .save_token(&StoredToken {
            access_token: "rejected".into(),
            authorization_id: Some("grant".into()),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now().timestamp() + 3_600),
        })
        .unwrap();
    let mut request = manager.begin_request();
    assert_eq!(
        request.credentials().await.unwrap().access_token,
        "rejected"
    );
    let previous_scope = manager
        .observe_credential_identity()
        .await
        .unwrap()
        .unwrap();
    assert!(request.reject_credentials());
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    assert!(request.credentials().await.is_err());
    server.verify().await;
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "replacement",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    let credentials = request.credentials().await.unwrap();
    let refreshed_scope = credentials.cache_identity(&manager.credential_id());
    assert_ne!(refreshed_scope, previous_scope);
    assert_eq!(
        manager.observe_credential_identity().await.unwrap(),
        Some(refreshed_scope)
    );
    assert_eq!(
        (credentials.access_token, credentials.authorization_id),
        ("replacement".into(), Some("grant".into()))
    );
    assert_eq!(manager.authorization_id().await.unwrap(), "grant");
    // Ordinary transport retries reuse the refreshed token without another exchange.
    assert_eq!(
        request.credentials().await.unwrap().access_token,
        "replacement"
    );
}

#[tokio::test]
async fn cache_scope_observation_does_not_wait_for_the_shared_credential_store() {
    let server = MockServer::start().await;
    let (manager, home) = client(config(&server), Arc::new(MockKeyringStore::default()));
    manager
        .save_token(&StoredToken {
            access_token: "test-observed-token".into(),
            authorization_id: Some("test-grant".into()),
            refresh_token: None,
            expires_at: Some(i64::MAX),
        })
        .unwrap();
    let expected = manager.observe_credential_identity().await.unwrap();
    assert!(expected.is_some());
    let mut other_config = config(&server);
    other_config.client_id = "unrelated-client".into();
    let other = OAuthSessionManager::new(
        other_config,
        home.path().to_path_buf(),
        transport_default_auth_route_config().http_client_factory(),
        Arc::new(MockKeyringStore::default()),
    )
    .unwrap();
    let store_guard = super::super::storage::lock_credentials(home.path())
        .await
        .unwrap();
    for observed in [&manager, &other] {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(250),
                observed.observe_credential_identity()
            )
            .await
            .unwrap()
            .unwrap(),
            None
        );
        assert!(observed.state.cached_token.try_lock().is_ok());
    }
    drop(store_guard);
    assert_eq!(
        manager.observe_credential_identity().await.unwrap(),
        expected
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
