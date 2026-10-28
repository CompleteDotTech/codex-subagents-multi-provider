//! Repeated close must use actual completion of the same writer acquisition.
use super::*;
use crate::InMemoryThreadStore;
use crate::NativeAuthorityEpoch;
use crate::ThreadPersistenceMetadata;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use futures::poll;

fn params(thread_id: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "shutdown-completion-test".into(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test-provider".into(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

#[tokio::test]
async fn completed_shutdown_is_idempotent_without_reopening_retired_writer() {
    let store = Arc::new(InMemoryThreadStore::default());
    let thread_id = ThreadId::new();
    let old = LiveThread::create_owned(store.clone(), params(thread_id))
        .await
        .unwrap();
    old.prepare_shutdown().await.unwrap();
    old.shutdown().await.unwrap();
    let completed_calls = store.calls().await;
    old.clone().prepare_shutdown().await.unwrap();
    old.completed_shutdown().await.unwrap();
    old.shutdown().await.unwrap();
    assert_eq!(
        store.calls().await.shutdown_thread,
        completed_calls.shutdown_thread
    );
    assert!(old.flush().await.is_err());
    assert!(old.persist(PersistContext::Standard).await.is_err());

    // A receipt belongs to an allocation, not its thread ID. It must neither qualify
    // a new writer's shutdown nor retire the replacement through an old clone.
    let (replacement, _) = LiveThread::resume_owned(
        store.clone(),
        ThreadHistoryMode::Legacy,
        ResumeThreadParams {
            thread_id,
            rollout_path: None,
            history: None,
            history_revision: None,
            include_archived: true,
            metadata: params(thread_id).metadata,
        },
        None,
    )
    .await
    .unwrap();
    assert!(replacement.completed_shutdown().await.is_err());
    old.prepare_shutdown().await.unwrap();
    old.shutdown().await.unwrap();
    replacement.prepare_shutdown().await.unwrap();
    replacement.flush().await.unwrap();
    replacement.shutdown().await.unwrap();
    assert_eq!(
        store.calls().await.shutdown_thread,
        completed_calls.shutdown_thread + 1
    );
}

#[tokio::test]
async fn cancelled_shutdown_waiter_retains_gate_until_real_writer_finishes() {
    let store = Arc::new(InMemoryThreadStore::default());
    let live = LiveThread::create_owned(store.clone(), params(ThreadId::new()))
        .await
        .unwrap();
    live.prepare_shutdown().await.unwrap();
    // Block the real admitted metadata operation, not a synthetic completion flag.
    let metadata = live.metadata_sync.lock().await;
    let mut shutdown = Box::pin(live.shutdown());
    assert!(poll!(shutdown.as_mut()).is_pending());
    // The spawned owner now retains both the shutdown gate and admitted writer work.
    drop(shutdown);
    let mut prepare = Box::pin(live.prepare_shutdown());
    assert!(poll!(prepare.as_mut()).is_pending());
    assert_eq!(store.calls().await.shutdown_thread, 0);
    drop(metadata);
    prepare.await.unwrap();
    live.completed_shutdown().await.unwrap();
    assert_eq!(store.calls().await.shutdown_thread, 1);
    assert!(live.flush().await.is_err());
}

#[tokio::test]
async fn failed_shutdown_receipt_preserves_error_and_allows_exact_cleanup_retry() {
    let store = Arc::new(InMemoryThreadStore::default());
    let thread_id = ThreadId::new();
    let live = LiveThread::create_owned(store.clone(), params(thread_id))
        .await
        .unwrap();
    let mut guard = LiveThreadInitGuard::new(Some(live.clone()));
    let epoch = NativeAuthorityEpoch {
        version: crate::NATIVE_AUTHORITY_VERSION,
        thread_id,
        root_thread_id: thread_id,
        parent_thread_id: None,
        generation: uuid::Uuid::now_v7(),
    };
    live.begin_native_authority_epoch(epoch.clone())
        .await
        .unwrap();
    let mut conflicting = epoch.clone();
    conflicting.generation = uuid::Uuid::now_v7();
    store
        .begin_native_authority_epoch(conflicting)
        .await
        .unwrap();
    // A genuine generation CAS failure leaves the writer closing and owned.
    let failure = live.shutdown().await.unwrap_err().to_string();
    assert_eq!(
        live.completed_shutdown().await.unwrap_err().to_string(),
        failure
    );
    assert_eq!(
        live.prepare_shutdown().await.unwrap_err().to_string(),
        failure
    );
    assert!(live.flush().await.is_err());
    assert!(
        LiveThread::create_owned(store.clone(), params(thread_id))
            .await
            .is_err()
    );
    assert_eq!(store.calls().await.discard_thread, 0);

    // Restore the actual generation, then exercise the original same-intent retry
    // contract. Metadata admission remains rejected; cleanup can nevertheless retire
    // its exact writer, and that error must not become an invented successful receipt.
    store.begin_native_authority_epoch(epoch).await.unwrap();
    assert!(live.shutdown().await.is_err());
    assert_eq!(
        live.completed_shutdown().await.unwrap_err().to_string(),
        failure
    );
    assert_eq!(
        live.prepare_shutdown().await.unwrap_err().to_string(),
        failure
    );
    assert_eq!(store.calls().await.discard_thread, 1);
    let replacement = LiveThread::create_owned(store.clone(), params(thread_id))
        .await
        .unwrap();
    guard.commit();
    replacement.prepare_shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_shutdown_before_admission_does_not_retire_writer() {
    let store = Arc::new(InMemoryThreadStore::default());
    let live = LiveThread::create_owned(store.clone(), params(ThreadId::new()))
        .await
        .unwrap();
    let operation = live.owner.as_ref().unwrap().operation().await.unwrap();
    let mut shutdown = Box::pin(live.shutdown());
    assert!(poll!(shutdown.as_mut()).is_pending());
    drop(shutdown);
    drop(operation);
    live.prepare_shutdown().await.unwrap();
    assert!(live.completed_shutdown().await.is_err());
    assert_eq!(store.calls().await.shutdown_thread, 0);
    live.shutdown().await.unwrap();
    live.completed_shutdown().await.unwrap();
    assert_eq!(store.calls().await.shutdown_thread, 1);
}
