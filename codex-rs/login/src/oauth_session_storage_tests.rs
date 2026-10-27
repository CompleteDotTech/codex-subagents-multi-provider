//! Login cancellation and readiness stay responsive during blocked credential reads.

use super::*;
use codex_keyring_store::CredentialStoreError;
use codex_keyring_store::KeyringStore;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Debug)]
struct BlockingKeyring {
    inner: MockKeyringStore,
    async_thread: std::thread::ThreadId,
    check_thread: AtomicBool,
    gate: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}

impl BlockingKeyring {
    fn check_thread(&self) {
        if self.check_thread.load(Ordering::Relaxed) {
            assert_ne!(std::thread::current().id(), self.async_thread);
        }
    }
}

impl KeyringStore for BlockingKeyring {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        self.check_thread();
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release
                .recv_timeout(Duration::from_secs(/*secs*/ 30))
                .unwrap();
        }
        self.inner.load(service, account)
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        self.check_thread();
        self.inner.save(service, account, value)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        self.check_thread();
        self.inner.delete(service, account)
    }
}

fn blocking_client() -> (OAuthSessionManager, tempfile::TempDir, Arc<BlockingKeyring>) {
    let home = tempfile::tempdir().unwrap();
    let keyring = Arc::new(BlockingKeyring {
        inner: MockKeyringStore::default(),
        async_thread: std::thread::current().id(),
        check_thread: AtomicBool::new(/*v*/ false),
        gate: Mutex::new(/*t*/ None),
    });
    let manager = OAuthSessionManager::new(
        loopback_config(),
        home.path().to_path_buf(),
        transport_default_auth_route_config().http_client_factory(),
        keyring.clone(),
    )
    .unwrap();
    (manager, home, keyring)
}

#[tokio::test]
async fn readiness_does_not_block_cached_requests_or_overwrite_newer_credentials() {
    let (manager, _home, keyring) = blocking_client();
    save_token(&manager, "old", /*refresh_token*/ None, i64::MAX);
    assert_eq!(manager.resolve_access_token().await.unwrap(), "old");
    keyring.check_thread.store(/*val*/ true, Ordering::Relaxed);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *keyring.gate.lock().unwrap() = Some((entered_tx, release_rx));
    let observer = manager.clone();
    let readiness = tokio::spawn(async move { observer.status().await });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), entered_rx)
        .await
        .unwrap()
        .unwrap();
    // The status read has captured the old encrypted file and is blocked in keyring I/O.
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 2),
            manager.resolve_access_token()
        )
        .await
        .unwrap()
        .unwrap(),
        "old"
    );
    let writer = manager.clone();
    tokio::task::spawn_blocking(move || {
        save_token(
            &writer,
            "replacement",
            /*refresh_token*/ None,
            i64::MAX,
        );
    })
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(
            // Refresh decrypts the persisted replacement; only cached requests must be immediate.
            Duration::from_secs(/*secs*/ 10),
            manager.refresh_access_token("old"),
        )
        .await
        .unwrap()
        .unwrap(),
        "replacement"
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        readiness.await.unwrap().unwrap(),
        crate::OAuthSessionStatus::Succeeded
    );
    assert_eq!(manager.resolve_access_token().await.unwrap(), "replacement");
}

#[tokio::test]
async fn cancel_during_keyring_read_releases_login_and_cached_credentials() {
    let (manager, _home, keyring) = blocking_client();
    save_token(&manager, "existing", /*refresh_token*/ None, i64::MAX);
    assert_eq!(manager.resolve_access_token().await.unwrap(), "existing");
    let mut events = manager.state.status_tx.subscribe();
    keyring.check_thread.store(/*val*/ true, Ordering::Relaxed);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *keyring.gate.lock().unwrap() = Some((entered_tx, release_rx));
    let cancel = Arc::new(tokio::sync::Notify::new());
    let login_manager = manager.clone();
    let login_cancel = Arc::clone(&cancel);
    let login = tokio::spawn(async move {
        login_manager
            .login_with_browser(login_cancel.notified(), |_| panic!("unexpected browser"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 2),
        manager.resolve_access_token(),
    )
    .await
    .expect("requests must not wait for login credential I/O")
    .unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<crate::OAuthSessionError>()),
        Some(&crate::OAuthSessionError::LoginInProgress),
    );
    assert_eq!(
        manager.status().await.unwrap(),
        crate::OAuthSessionStatus::Started
    );
    assert_eq!(
        events.try_recv().unwrap().status,
        crate::OAuthSessionStatus::Started
    );
    cancel.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), login)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(
        events.try_recv().unwrap().status,
        crate::OAuthSessionStatus::Failed {
            message: "Gateway sign-in was canceled".into(),
        }
    );
    // Both request consumers and a new login must proceed before the keyring read returns.
    assert_eq!(
        tokio::time::timeout(
            // Failed status makes the request reread and decrypt the stored credential.
            Duration::from_secs(/*secs*/ 10),
            manager.resolve_access_token()
        )
        .await
        .unwrap()
        .unwrap(),
        "existing"
    );
    let error = manager
        .login_with_browser(std::future::ready(()), |_| panic!("unexpected browser"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    release_tx.send(()).unwrap();
}

async fn blocked_credential_observation(
    manager: &OAuthSessionManager,
    keyring: &BlockingKeyring,
) -> (
    tokio::task::JoinHandle<std::io::Result<Option<String>>>,
    std::sync::mpsc::Sender<()>,
) {
    keyring.check_thread.store(true, Ordering::Relaxed);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *keyring.gate.lock().unwrap() = Some((entered_tx, release_rx));
    let observer = manager.clone();
    let observation = tokio::spawn(async move { observer.observe_credential_identity().await });
    tokio::time::timeout(Duration::from_secs(2), entered_rx)
        .await
        .unwrap()
        .unwrap();
    (observation, release_tx)
}

#[tokio::test]
async fn credential_observation_rejects_cache_changes_during_keyring_read() {
    for change in ["pending", "rejected", "replacement"] {
        let (manager, _home, keyring) = blocking_client();
        save_token(&manager, "existing", None, i64::MAX);
        assert_eq!(manager.resolve_access_token().await.unwrap(), "existing");
        let token = manager
            .state
            .cached_token
            .lock()
            .await
            .token
            .clone()
            .unwrap();
        let (observation, release) = blocked_credential_observation(&manager, &keyring).await;
        {
            let mut cache = manager.state.cached_token.try_lock().unwrap();
            match change {
                "pending" => cache.pending = Some(token.clone()),
                "rejected" => cache.login_required = Some(token.clone()),
                "replacement" => {
                    cache.token = Some(StoredToken {
                        access_token: "newer".into(),
                        ..token
                    });
                }
                _ => unreachable!(),
            }
        }
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), observation)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn credential_observation_rejects_status_aba_during_keyring_read() {
    let (manager, _home, keyring) = blocking_client();
    save_token(&manager, "existing", None, i64::MAX);
    assert_eq!(manager.resolve_access_token().await.unwrap(), "existing");
    let token = manager
        .state
        .cached_token
        .lock()
        .await
        .token
        .clone()
        .unwrap();
    manager.publish_status(crate::OAuthSessionStatus::Succeeded, Some(&token));
    let (observation, release) = blocked_credential_observation(&manager, &keyring).await;
    // Both the cache and final status/credential return to their initial values.
    // Only the consumed watch revision reveals the intervening login attempt.
    manager.publish_status(crate::OAuthSessionStatus::Started, None);
    manager.publish_status(crate::OAuthSessionStatus::Succeeded, Some(&token));
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), observation)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        None
    );
    // A new observation after the race can prove the actual request credential.
    let observed = manager
        .observe_credential_identity()
        .await
        .unwrap()
        .unwrap();
    let credentials = manager.begin_request().credentials().await.unwrap();
    assert!(observed == credentials.cache_identity(&manager.credential_id()));
}

#[tokio::test]
async fn credential_observation_yields_store_to_logout_without_waiting_for_cache() {
    let (manager, _home, keyring) = blocking_client();
    save_token(&manager, "existing", None, i64::MAX);
    assert_eq!(manager.resolve_access_token().await.unwrap(), "existing");
    let (observation, release) = blocked_credential_observation(&manager, &keyring).await;
    let writer = manager.clone();
    let logout = tokio::spawn(async move { writer.logout().await });
    // The real mutator owns cache while waiting for the observer's disk lock.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager.state.cached_token.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), observation)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        None
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), logout)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert!(manager.load_token_async().await.unwrap().is_none());
    assert!(
        manager
            .state
            .cached_token
            .try_lock()
            .unwrap()
            .token
            .is_none()
    );
}

#[tokio::test]
async fn cancelled_credential_observation_retains_store_but_not_cache_until_read_finishes() {
    let (manager, _home, keyring) = blocking_client();
    save_token(&manager, "existing", None, i64::MAX);
    assert_eq!(manager.resolve_access_token().await.unwrap(), "existing");
    let mut status_changes = manager.state.status.subscribe();
    drop(status_changes.borrow_and_update());
    let mut events = manager.state.status_tx.subscribe();
    let (observation, release) = blocked_credential_observation(&manager, &keyring).await;
    observation.abort();
    assert!(observation.await.unwrap_err().is_cancelled());
    assert!(manager.state.cached_token.try_lock().is_ok());
    assert!(
        super::super::storage::try_lock_credentials(&manager.state.codex_home)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), manager.resolve_access_token())
            .await
            .unwrap()
            .unwrap(),
        "existing"
    );
    release.send(()).unwrap();
    // Acquiring the very same cooperating lock proves the detached read worker
    // has finished validation and released ownership after caller cancellation.
    let store = tokio::time::timeout(
        Duration::from_secs(10),
        super::super::storage::lock_credentials(&manager.state.codex_home),
    )
    .await
    .unwrap()
    .unwrap();
    drop(store);
    assert!(!status_changes.has_changed().unwrap());
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}
