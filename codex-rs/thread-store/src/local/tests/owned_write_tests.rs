//! A retired owned writer must not flush its retained metadata into a replacement.

use super::*;
use crate::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct MetadataFailureStore {
    inner: LocalThreadStore,
    fail_metadata: AtomicBool,
    injected_failures: AtomicUsize,
}

macro_rules! delegate_owned_write_store {
    ($(fn $name:ident($arg:ident: $ty:ty) -> $output:ty;)*) => {
        $(fn $name(&self, $arg: $ty) -> ThreadStoreFuture<'_, $output> {
            <LocalThreadStore as ThreadStore>::$name(&self.inner, $arg)
        })*
    };
}

impl ThreadStore for MetadataFailureStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate_owned_write_store! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn create_thread_owned(params: CreateThreadParams) -> ThreadWriterOwner;
        fn resume_thread(params: ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: AppendThreadItemsParams) -> ();
        fn flush_thread(thread: ThreadId) -> ();
        fn shutdown_thread(thread: ThreadId) -> ();
        fn discard_thread(thread: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
    }

    fn persist_thread(
        &self,
        thread: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread, context)
    }

    fn resume_thread_owned(
        &self,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, ThreadWriterOwner)> {
        self.inner.resume_thread_owned(params, preparation)
    }

    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: ThreadWriterOwner,
        discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            if self.fail_metadata.load(Ordering::SeqCst) {
                self.injected_failures.fetch_add(1, Ordering::SeqCst);
                return Err(ThreadStoreError::Internal {
                    message: "injected owned metadata write failure".into(),
                });
            }
            self.inner.update_thread_metadata(params).await
        })
    }
}

fn owned_user_message(mode: ThreadHistoryMode, thread: ThreadId, text: &str) -> RolloutItem {
    match mode {
        ThreadHistoryMode::Legacy => user_message_item(text),
        ThreadHistoryMode::Paginated => {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: thread,
                turn_id: "owned-write-retry-turn".into(),
                item: TurnItem::UserMessage(UserMessageItem {
                    id: format!("owned-write-{text}"),
                    client_id: None,
                    content: vec![codex_protocol::user_input::UserInput::Text {
                        text: text.into(),
                        text_elements: Vec::new(),
                    }],
                }),
                started_at_ms: Some(0),
                completed_at_ms: 1,
            }))
        }
    }
}

async fn assert_owned_shutdown_retry_preserves_replacement(mode: ThreadHistoryMode) {
    let home = TempDir::new().expect("owned temporary home");
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("actual metadata SQLite store");
    let store = Arc::new(MetadataFailureStore {
        inner: LocalThreadStore::new(config, Some(state.clone())),
        fail_metadata: AtomicBool::new(false),
        injected_failures: AtomicUsize::new(0),
    });
    let thread = ThreadId::default();
    let original_message = owned_user_message(mode, thread, "retired original preview");
    let replacement_message = owned_user_message(mode, thread, "replacement writer remains usable");
    for item in [&original_message, &replacement_message] {
        assert_eq!(
            codex_rollout::persisted_rollout_items(std::slice::from_ref(item), mode).len(),
            1,
            "actual selected history mode must retain each owned user message"
        );
    }
    let mut params = create_thread_params(thread);
    params.history_mode = mode;
    params.metadata.cwd = Some(home.path().to_path_buf());
    let writer_store: Arc<dyn ThreadStore> = store.clone();
    let old = LiveThread::create_owned(writer_store, params)
        .await
        .expect("actual original owned writer");
    old.persist(PersistContext::Standard)
        .await
        .expect("materialize actual original rollout and creation metadata");
    old.flush().await.expect("initial actual durable writer");
    let path = store.inner.live_rollout_path(thread).await.unwrap();
    assert!(path.exists());

    // The append reaches the real writer first. Only its metadata update fails,
    // leaving an actual append-derived patch retained by the original LiveThread.
    store.fail_metadata.store(true, Ordering::SeqCst);
    let appended = old.append_items(&[original_message]).await;
    assert!(
        matches!(appended, Err(ThreadStoreError::Internal { ref message }) if message == "injected owned metadata write failure")
    );
    let shutdown = old.shutdown().await;
    assert!(
        matches!(shutdown, Err(ThreadStoreError::Internal { ref message }) if message == "injected owned metadata write failure")
    );
    assert_eq!(store.injected_failures.load(Ordering::SeqCst), 2);
    assert!(
        !store
            .inner
            .live_recorders
            .lock()
            .await
            .contains_key(&thread),
        "metadata failure must have still completed actual exact writer shutdown"
    );
    assert!(
        path.exists(),
        "saved original history must survive shutdown"
    );

    store.fail_metadata.store(false, Ordering::SeqCst);
    let writer_store: Arc<dyn ThreadStore> = store.clone();
    let (replacement, history) = LiveThread::resume_owned(
        writer_store,
        mode,
        ResumeThreadParams {
            thread_id: thread,
            rollout_path: Some(path.clone()),
            history: None,
            history_revision: None,
            include_archived: true,
            metadata: thread_metadata(),
        },
        /*preparation*/ None,
    )
    .await
    .expect("actual replacement acquires the released writer");
    assert!(
        serde_json::to_string(&history)
            .unwrap()
            .contains("retired original preview")
    );
    replacement
        .update_metadata(
            ThreadMetadataPatch {
                preview: Some("current replacement preview".into()),
                ..Default::default()
            },
            /*include_archived*/ true,
        )
        .await
        .expect("distinguishable actual replacement metadata");
    replacement.flush().await.unwrap();
    let before = replacement
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .unwrap();
    if mode == ThreadHistoryMode::Paginated {
        assert_eq!(before.preview, "current replacement preview");
    }
    // Legacy read_thread derives preview from rollout; the canonical SQLite row
    // is the actual metadata target in both modes, independent of read projection.
    let metadata_before = state.get_thread(thread).await.unwrap().unwrap();
    assert_eq!(
        metadata_before.preview.as_deref(),
        Some("current replacement preview")
    );
    // Read the entire actual owned rollout, including Paginated records; the
    // metadata API deliberately rejects legacy full-history reads in that mode.
    let (history_before, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
        .await
        .unwrap();
    let history_before = serde_json::to_value(&history_before).unwrap();

    // Retrying an old handle may reject its stale identity or safely no-op, but
    // must not write its retained patch before the exact owner check.
    let retry = old.shutdown().await;
    replacement.flush().await.unwrap();
    let after = replacement
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .unwrap();
    let metadata_after = state.get_thread(thread).await.unwrap().unwrap();
    let (history_after, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
        .await
        .unwrap();
    let history_after = serde_json::to_value(&history_after).unwrap();
    let usable = replacement.append_items(&[replacement_message]).await;
    let flushed = replacement.flush().await;
    let (saved, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
        .await
        .unwrap();
    let usable_message_saved = serde_json::to_string(&saved)
        .unwrap()
        .contains("replacement writer remains usable");
    // Close the actual replacement before asserting the captured regression,
    // including on the expected RED outcome, to avoid retaining a test writer.
    replacement.shutdown().await.unwrap();
    assert!(
        usable.is_ok(),
        "stale shutdown must preserve replacement writer: {retry:?}"
    );
    assert!(flushed.is_ok());
    assert!(usable_message_saved);
    assert_eq!(
        history_after, history_before,
        "stale owned shutdown changed saved replacement history"
    );
    assert_eq!(
        after.preview, before.preview,
        "retired owned shutdown replayed its metadata into the replacement"
    );
    assert_eq!(
        metadata_after.preview, metadata_before.preview,
        "retired owned shutdown changed canonical SQLite replacement preview"
    );
}

#[tokio::test]
async fn owned_shutdown_metadata_retry_preserves_legacy_replacement() {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        assert_owned_shutdown_retry_preserves_replacement(ThreadHistoryMode::Legacy),
    )
    .await
    .expect("bounded original shutdown and replacement retry");
}

#[tokio::test]
async fn owned_shutdown_metadata_retry_preserves_paginated_replacement() {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        assert_owned_shutdown_retry_preserves_replacement(ThreadHistoryMode::Paginated),
    )
    .await
    .expect("bounded original shutdown and replacement retry");
}

#[derive(Default)]
struct WriterMetadataGate {
    armed: AtomicBool,
    panic_after_commit: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct GatedWriterStore {
    inner: LocalThreadStore,
    gate: Arc<WriterMetadataGate>,
}
impl ThreadStore for GatedWriterStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate_owned_write_store! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn create_thread_owned(params: CreateThreadParams) -> ThreadWriterOwner;
        fn resume_thread(params: ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: AppendThreadItemsParams) -> ();
        fn flush_thread(thread: ThreadId) -> ();
        fn shutdown_thread(thread: ThreadId) -> ();
        fn discard_thread(thread: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
    }

    fn persist_thread(
        &self,
        thread: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread, context)
    }

    fn resume_thread_owned(
        &self,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, ThreadWriterOwner)> {
        self.inner.resume_thread_owned(params, preparation)
    }

    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: ThreadWriterOwner,
        discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            self.inner.update_thread_metadata(params).await
        })
    }
}

async fn poll_writer_blocked<F: std::future::Future>(
    future: std::pin::Pin<&mut F>,
    owner: &ThreadWriterOwner,
) {
    let observed = owner.watch_lease_wait();
    let mut future = future;
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(_) => panic!("operation passed the held writer lease"),
    })
    .await;
    observed.notified().await; // actual lock acquisition polled Pending, not an outer task/IO yield
}

#[tokio::test]
async fn owned_mutation_cancellation_drains_admitted_work_before_retirement() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let home = TempDir::new().unwrap();
        let config = test_config(home.path());
        let state = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let gate = Arc::new(WriterMetadataGate::default());
        let store = Arc::new(GatedWriterStore {
            inner: LocalThreadStore::new(config, Some(state.clone())),
            gate: gate.clone(),
        });
        let thread = ThreadId::default();
        let mut params = create_thread_params(thread);
        params.history_mode = ThreadHistoryMode::Paginated;
        let live = LiveThread::create_owned(store.clone(), params)
            .await
            .unwrap();
        live.persist(PersistContext::Standard).await.unwrap();
        gate.armed.store(true, Ordering::SeqCst);
        let writer = live.clone();
        let waiter = tokio::spawn(async move {
            writer
                .update_metadata(
                    ThreadMetadataPatch {
                        preview: Some("admitted write".into()),
                        ..Default::default()
                    },
                    /*include_archived*/ true,
                )
                .await
        });
        gate.entered.notified().await; // actual adapter await, inside admitted lease
        let owner = store
            .inner
            .live_recorders
            .lock()
            .await
            .get(&thread)
            .unwrap()
            .owner
            .clone();
        let mut never_admitted = Box::pin(live.update_metadata(
            ThreadMetadataPatch {
                preview: Some("cancelled before admission".into()),
                ..Default::default()
            },
            /*include_archived*/ true,
        ));
        poll_writer_blocked(never_admitted.as_mut(), &owner).await;
        drop(never_admitted);
        let mut finish = Box::pin(
            store
                .inner
                .finish_thread_owned(thread, owner.clone(), false),
        );
        poll_writer_blocked(finish.as_mut(), &owner).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        // The outer waiter is gone; the actual admitted I/O still owns the lease.
        poll_writer_blocked(finish.as_mut(), &owner).await;
        gate.release.notify_one();
        finish.await.unwrap();
        assert_eq!(
            state
                .get_thread(thread)
                .await
                .unwrap()
                .unwrap()
                .preview
                .as_deref(),
            Some("admitted write")
        );
        assert!(live.flush().await.is_err());
        assert!(
            !store
                .inner
                .live_recorders
                .lock()
                .await
                .contains_key(&thread)
        );
    })
    .await
    .expect("bounded admitted mutation and retirement drain");
}

#[tokio::test]
async fn retired_owned_handle_rejects_every_mutation_and_preserves_replacement() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(LocalThreadStore::new(config, Some(state.clone())));
    let thread = ThreadId::default();
    let mut params = create_thread_params(thread);
    params.history_mode = ThreadHistoryMode::Paginated;
    let old = LiveThread::create_owned(store.clone(), params)
        .await
        .unwrap();
    old.append_items(&[owned_user_message(
        ThreadHistoryMode::Paginated,
        thread,
        "saved original",
    )])
    .await
    .unwrap();
    old.persist(PersistContext::Standard).await.unwrap();
    old.shutdown().await.unwrap();
    let metadata = state.get_thread(thread).await.unwrap().unwrap();
    let path = metadata.rollout_path;
    let (replacement, _) = LiveThread::resume_owned(
        store.clone(),
        ThreadHistoryMode::Paginated,
        ResumeThreadParams {
            thread_id: thread,
            rollout_path: Some(path.clone()),
            history: None,
            history_revision: None,
            include_archived: true,
            metadata: thread_metadata(),
        },
        /*preparation*/ None,
    )
    .await
    .unwrap();
    replacement
        .update_metadata(
            ThreadMetadataPatch {
                preview: Some("replacement".into()),
                ..Default::default()
            },
            /*include_archived*/ true,
        )
        .await
        .unwrap();
    replacement.flush().await.unwrap();
    let before = state.get_thread(thread).await.unwrap().unwrap();
    let memory_mode_before = state.get_thread_memory_mode(thread).await.unwrap();
    assert!(memory_mode_before.is_some());
    let bytes = tokio::fs::read(&path).await.unwrap();
    assert!(
        old.append_items(&[owned_user_message(
            ThreadHistoryMode::Paginated,
            thread,
            "stale"
        )])
        .await
        .is_err()
    );
    for context in [
        PersistContext::Standard,
        PersistContext::ThreadPreparation,
        PersistContext::SubagentSpawn,
        PersistContext::TurnStart,
        PersistContext::SteeredUserInput,
    ] {
        assert!(old.persist(context).await.is_err());
        replacement.persist(context).await.unwrap();
    }
    assert!(old.flush().await.is_err());
    assert!(
        old.update_memory_mode(
            codex_protocol::protocol::ThreadMemoryMode::Disabled,
            /*include_archived*/ true
        )
        .await
        .is_err()
    );
    assert!(
        old.update_metadata(
            ThreadMetadataPatch {
                preview: Some("stale".into()),
                ..Default::default()
            },
            /*include_archived*/ true
        )
        .await
        .is_err()
    );
    assert!(
        old.begin_native_authority_epoch(crate::NativeAuthorityEpoch {
            version: crate::NATIVE_AUTHORITY_VERSION,
            thread_id: thread,
            root_thread_id: thread,
            parent_thread_id: None,
            generation: uuid::Uuid::now_v7()
        })
        .await
        .is_err()
    );
    assert!(old.shutdown().await.is_err());
    old.discard().await.unwrap();
    replacement.flush().await.unwrap();
    let after = state.get_thread(thread).await.unwrap().unwrap();
    assert_eq!(after.preview, before.preview);
    assert_eq!(
        state.get_thread_memory_mode(thread).await.unwrap(),
        memory_mode_before
    );
    assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
    assert!(
        state
            .read_native_authority_epoch(thread)
            .await
            .unwrap()
            .is_none()
    );
    replacement
        .append_items(&[owned_user_message(
            ThreadHistoryMode::Paginated,
            thread,
            "usable",
        )])
        .await
        .unwrap();
    replacement.flush().await.unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_metadata_failure_remains_retryable_for_current_owner() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(MetadataFailureStore {
        inner: LocalThreadStore::new(config, Some(state.clone())),
        fail_metadata: AtomicBool::new(false),
        injected_failures: AtomicUsize::new(0),
    });
    let thread = ThreadId::default();
    let mut params = create_thread_params(thread);
    params.history_mode = ThreadHistoryMode::Paginated;
    let live = LiveThread::create_owned(store.clone(), params)
        .await
        .unwrap();
    live.persist(PersistContext::Standard).await.unwrap();
    store.fail_metadata.store(true, Ordering::SeqCst);
    assert!(
        live.append_items(&[owned_user_message(
            ThreadHistoryMode::Paginated,
            thread,
            "retry current owner"
        )])
        .await
        .is_err()
    );
    store.fail_metadata.store(false, Ordering::SeqCst);
    live.flush().await.unwrap();
    assert_eq!(store.injected_failures.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .get_thread(thread)
            .await
            .unwrap()
            .unwrap()
            .preview
            .as_deref(),
        Some("retry current owner")
    );
    live.update_memory_mode(
        codex_protocol::protocol::ThreadMemoryMode::Disabled,
        /*include_archived*/ true,
    )
    .await
    .unwrap();
    live.shutdown().await.unwrap();
}
#[tokio::test]
async fn owned_mutation_orders_active_single_and_batch_delete() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for batch in [false, true] {
            let home = TempDir::new().unwrap();
            let config = test_config(home.path());
            let state = codex_state::StateRuntime::init(
                config.sqlite.clone(),
                config.default_model_provider_id.clone(),
            )
            .await
            .unwrap();
            let gate = Arc::new(WriterMetadataGate::default());
            let store = Arc::new(GatedWriterStore {
                inner: LocalThreadStore::new(config, Some(state.clone())),
                gate: gate.clone(),
            });
            let thread = ThreadId::default();
            let mut params = create_thread_params(thread);
            params.history_mode = ThreadHistoryMode::Paginated;
            let live = LiveThread::create_owned(store.clone(), params)
                .await
                .unwrap();
            live.persist(PersistContext::Standard).await.unwrap();
            let path = store.inner.live_rollout_path(thread).await.unwrap();
            let peer_id = ThreadId::default();
            let survivor_id = ThreadId::default();
            let mut peer_params = create_thread_params(peer_id);
            peer_params.history_mode = ThreadHistoryMode::Paginated;
            let peer = LiveThread::create_owned(store.clone(), peer_params)
                .await
                .unwrap();
            peer.persist(PersistContext::Standard).await.unwrap();
            let mut survivor_params = create_thread_params(survivor_id);
            survivor_params.history_mode = ThreadHistoryMode::Paginated;
            let survivor = LiveThread::create_owned(store.clone(), survivor_params)
                .await
                .unwrap();
            survivor.persist(PersistContext::Standard).await.unwrap();
            let owner = store
                .inner
                .live_recorders
                .lock()
                .await
                .get(&thread)
                .unwrap()
                .owner
                .clone();
            gate.armed.store(true, Ordering::SeqCst);
            let writer = live.clone();
            let write = tokio::spawn(async move {
                writer
                    .update_metadata(
                        ThreadMetadataPatch {
                            preview: Some("before delete".into()),
                            ..Default::default()
                        },
                        /*include_archived*/ true,
                    )
                    .await
            });
            gate.entered.notified().await;
            let mut delete = Box::pin(async {
                if batch {
                    store
                        .inner
                        .delete_threads(DeleteThreadsParams {
                            thread_ids: vec![peer_id, thread, peer_id],
                        })
                        .await
                } else {
                    store
                        .inner
                        .delete_thread(DeleteThreadParams { thread_id: thread })
                        .await
                }
            });
            poll_writer_blocked(delete.as_mut(), &owner).await;
            gate.release.notify_one();
            write.await.unwrap().unwrap();
            delete.await.unwrap();
            assert!(!path.exists());
            assert!(state.get_thread(thread).await.unwrap().is_none());
            assert!(
                !store
                    .inner
                    .live_recorders
                    .lock()
                    .await
                    .contains_key(&thread)
            );
            assert!(
                live.append_items(&[owned_user_message(
                    ThreadHistoryMode::Paginated,
                    thread,
                    "after delete"
                )])
                .await
                .is_err()
            );
            assert!(state.get_thread(thread).await.unwrap().is_none());
            if batch {
                assert!(state.get_thread(peer_id).await.unwrap().is_none());
                assert!(peer.flush().await.is_err());
            } else {
                peer.append_items(&[owned_user_message(
                    ThreadHistoryMode::Paginated,
                    peer_id,
                    "peer unaffected",
                )])
                .await
                .unwrap();
                peer.shutdown().await.unwrap();
            }
            survivor
                .append_items(&[owned_user_message(
                    ThreadHistoryMode::Paginated,
                    survivor_id,
                    "unrelated survivor",
                )])
                .await
                .unwrap();
            survivor.flush().await.unwrap();
            survivor.shutdown().await.unwrap();
        }
    })
    .await
    .expect("bounded active deletes after admitted metadata work");
}

struct GatedMemoryWriterStore {
    owner: tokio::sync::Mutex<Option<ThreadWriterOwner>>,
    inner: InMemoryThreadStore,
    gate: Arc<WriterMetadataGate>,
}
impl ThreadStore for GatedMemoryWriterStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::create_thread(&self.inner, params)
    }
    fn create_thread_owned(
        &self,
        params: CreateThreadParams,
    ) -> ThreadStoreFuture<'_, ThreadWriterOwner> {
        Box::pin(async move {
            let owner =
                <InMemoryThreadStore as ThreadStore>::create_thread_owned(&self.inner, params)
                    .await?;
            *self.owner.lock().await = Some(owner.clone());
            Ok(owner)
        })
    }
    fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>> {
        <InMemoryThreadStore as ThreadStore>::resume_thread(&self.inner, params)
    }
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::append_items(&self.inner, params)
    }
    fn flush_thread(&self, thread: ThreadId) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::flush_thread(&self.inner, thread)
    }
    fn shutdown_thread(&self, thread: ThreadId) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::shutdown_thread(&self.inner, thread)
    }
    fn discard_thread(&self, thread: ThreadId) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::discard_thread(&self.inner, thread)
    }
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        <InMemoryThreadStore as ThreadStore>::load_history(&self.inner, params)
    }
    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        <InMemoryThreadStore as ThreadStore>::read_thread(&self.inner, params)
    }
    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        <InMemoryThreadStore as ThreadStore>::read_thread_by_rollout_path(&self.inner, params)
    }
    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        <InMemoryThreadStore as ThreadStore>::list_threads(&self.inner, params)
    }
    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::archive_thread(&self.inner, params)
    }
    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        <InMemoryThreadStore as ThreadStore>::unarchive_thread(&self.inner, params)
    }
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        <InMemoryThreadStore as ThreadStore>::delete_thread(&self.inner, params)
    }

    fn persist_thread(
        &self,
        thread: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread, context)
    }

    fn resume_thread_owned(
        &self,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, ThreadWriterOwner)> {
        self.inner.resume_thread_owned(params, preparation)
    }

    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: ThreadWriterOwner,
        discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            let result =
                <InMemoryThreadStore as ThreadStore>::update_thread_metadata(&self.inner, params)
                    .await?;
            if self.gate.panic_after_commit.swap(false, Ordering::SeqCst) {
                panic!("injected adapter panic after actual metadata commit");
            }
            Ok(result)
        })
    }
}

#[tokio::test]
async fn owned_memory_mutation_orders_legacy_overwrite_and_delete() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for action in 0..5 {
            let gate = Arc::new(WriterMetadataGate::default());
            let store = Arc::new(GatedMemoryWriterStore {
                owner: tokio::sync::Mutex::new(None),
                inner: InMemoryThreadStore::default(),
                gate: gate.clone(),
            });
            let thread = ThreadId::default();
            let live = LiveThread::create_owned(store.clone(), create_thread_params(thread))
                .await
                .unwrap();
            live.append_items(&[user_message_item("saved original")])
                .await
                .unwrap();
            let owner = store.owner.lock().await.clone().unwrap();
            store.inner.omit_metadata_update_result_for_testing();
            gate.armed.store(true, Ordering::SeqCst);
            let writer = live.clone();
            let write = tokio::spawn(async move {
                writer
                    .update_metadata(
                        ThreadMetadataPatch {
                            preview: Some("admitted memory".into()),
                            ..Default::default()
                        },
                        /*include_archived*/ true,
                    )
                    .await
            });
            gate.entered.notified().await;
            let mut lifecycle = Box::pin(async {
                match action {
                    0 => {
                        <InMemoryThreadStore as ThreadStore>::create_thread(
                            &store.inner,
                            create_thread_params(thread),
                        )
                        .await
                    }
                    1 => <InMemoryThreadStore as ThreadStore>::resume_thread(
                        &store.inner,
                        ResumeThreadParams {
                            thread_id: thread,
                            rollout_path: None,
                            history: None,
                            history_revision: None,
                            include_archived: true,
                            metadata: thread_metadata(),
                        },
                    )
                    .await
                    .map(|_| ()),
                    2 => {
                        <InMemoryThreadStore as ThreadStore>::delete_thread(
                            &store.inner,
                            DeleteThreadParams { thread_id: thread },
                        )
                        .await
                    }
                    3 => {
                        <InMemoryThreadStore as ThreadStore>::shutdown_thread(&store.inner, thread)
                            .await
                    }
                    _ => {
                        <InMemoryThreadStore as ThreadStore>::discard_thread(&store.inner, thread)
                            .await
                    }
                }
            });
            poll_writer_blocked(lifecycle.as_mut(), &owner).await;
            gate.release.notify_one();
            assert_eq!(write.await.unwrap().unwrap().preview, "admitted memory");
            lifecycle.await.unwrap();
            assert!(
                live.append_items(&[user_message_item("stale memory")])
                    .await
                    .is_err()
            );
            assert!(live.flush().await.is_err());
            assert!(
                live.update_metadata(
                    ThreadMetadataPatch {
                        preview: Some("stale memory".into()),
                        ..Default::default()
                    },
                    /*include_archived*/ true
                )
                .await
                .is_err()
            );
            if action == 2 {
                assert!(
                    <InMemoryThreadStore as ThreadStore>::read_thread(
                        &store.inner,
                        ReadThreadParams {
                            thread_id: thread,
                            include_archived: true,
                            include_history: false
                        }
                    )
                    .await
                    .is_err()
                );
                let new = LiveThread::create_owned(store.clone(), create_thread_params(thread))
                    .await
                    .unwrap();
                new.append_items(&[user_message_item("new after delete")])
                    .await
                    .unwrap();
                new.shutdown().await.unwrap();
            } else {
                let before = <InMemoryThreadStore as ThreadStore>::load_history(
                    &store.inner,
                    LoadThreadHistoryParams {
                        thread_id: thread,
                        include_archived: true,
                    },
                )
                .await
                .unwrap();
                live.discard().await.unwrap();
                let after = <InMemoryThreadStore as ThreadStore>::load_history(
                    &store.inner,
                    LoadThreadHistoryParams {
                        thread_id: thread,
                        include_archived: true,
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    serde_json::to_value(after.items).unwrap(),
                    serde_json::to_value(before.items).unwrap()
                );
                <InMemoryThreadStore as ThreadStore>::append_items(
                    &store.inner,
                    AppendThreadItemsParams {
                        thread_id: thread,
                        items: vec![user_message_item("legacy replacement usable")],
                    },
                )
                .await
                .unwrap();
                <InMemoryThreadStore as ThreadStore>::shutdown_thread(&store.inner, thread)
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .expect("bounded actual in-memory replacement/deletion races");
}

#[tokio::test]
async fn owned_finish_failure_keeps_closing_writer_until_exact_retry() {
    let store = Arc::new(InMemoryThreadStore::default());
    let thread = ThreadId::default();
    let live = LiveThread::create_owned(store.clone(), create_thread_params(thread))
        .await
        .unwrap();
    let mut guard = LiveThreadInitGuard::new(Some(live.clone()));
    let first = crate::NativeAuthorityEpoch {
        version: crate::NATIVE_AUTHORITY_VERSION,
        thread_id: thread,
        root_thread_id: thread,
        parent_thread_id: None,
        generation: uuid::Uuid::now_v7(),
    };
    live.begin_native_authority_epoch(first.clone())
        .await
        .unwrap();
    let mut conflicting = first.clone();
    conflicting.generation = uuid::Uuid::now_v7();
    // Actual authority generation CAS mismatch makes provisional teardown fail.
    store
        .begin_native_authority_epoch(conflicting.clone())
        .await
        .unwrap();
    assert!(guard.discard().await.is_err());
    assert!(
        live.append_items(&[user_message_item("after failed cleanup")])
            .await
            .is_err()
    );
    assert!(
        LiveThread::create_owned(store.clone(), create_thread_params(thread))
            .await
            .is_err()
    );
    assert!(
        <InMemoryThreadStore as ThreadStore>::create_thread(
            store.as_ref(),
            create_thread_params(thread)
        )
        .await
        .is_err()
    );
    let retained = serde_json::to_value(
        <InMemoryThreadStore as ThreadStore>::load_history(
            store.as_ref(),
            LoadThreadHistoryParams {
                thread_id: thread,
                include_archived: true,
            },
        )
        .await
        .unwrap()
        .items,
    )
    .unwrap();
    assert!(store.shutdown_thread(thread).await.is_err());
    assert!(store.discard_thread(thread).await.is_err());
    assert!(
        LiveThread::resume_owned(
            store.clone(),
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id: thread,
                rollout_path: None,
                history: None,
                history_revision: None,
                include_archived: true,
                metadata: thread_metadata(),
            },
            /*preparation*/ None
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .read_native_authority_epoch(thread)
            .await
            .unwrap()
            .unwrap()
            .generation,
        conflicting.generation
    );
    assert_eq!(
        serde_json::to_value(
            <InMemoryThreadStore as ThreadStore>::load_history(
                store.as_ref(),
                LoadThreadHistoryParams {
                    thread_id: thread,
                    include_archived: true
                }
            )
            .await
            .unwrap()
            .items
        )
        .unwrap(),
        retained
    );
    store.begin_native_authority_epoch(first).await.unwrap();
    guard.discard().await.unwrap();
    let replacement = LiveThread::create_owned(store.clone(), create_thread_params(thread))
        .await
        .unwrap();
    replacement
        .append_items(&[user_message_item("replacement after exact retry")])
        .await
        .unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_operation_panic_after_real_commit_fails_closed() {
    let gate = Arc::new(WriterMetadataGate::default());
    let store = Arc::new(GatedMemoryWriterStore {
        owner: tokio::sync::Mutex::new(None),
        inner: InMemoryThreadStore::default(),
        gate: gate.clone(),
    });
    let thread = ThreadId::default();
    let live = LiveThread::create_owned(store.clone(), create_thread_params(thread))
        .await
        .unwrap();
    live.persist(PersistContext::Standard).await.unwrap();
    gate.panic_after_commit.store(true, Ordering::SeqCst);
    assert!(
        live.update_metadata(
            ThreadMetadataPatch {
                preview: Some("committed before panic".into()),
                ..Default::default()
            },
            /*include_archived*/ true
        )
        .await
        .is_err()
    );
    let actual = <InMemoryThreadStore as ThreadStore>::read_thread(
        &store.inner,
        ReadThreadParams {
            thread_id: thread,
            include_archived: true,
            include_history: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(actual.preview, "committed before panic");
    assert!(live.flush().await.is_err());
    assert!(live.shutdown().await.is_err());
    assert!(live.discard().await.is_err());
    assert!(
        LiveThread::create_owned(store.clone(), create_thread_params(thread))
            .await
            .is_err()
    );
    assert!(
        <InMemoryThreadStore as ThreadStore>::create_thread(
            &store.inner,
            create_thread_params(thread)
        )
        .await
        .is_err()
    );
    assert!(
        <InMemoryThreadStore as ThreadStore>::delete_thread(
            &store.inner,
            DeleteThreadParams { thread_id: thread }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn owned_local_mutation_orders_legacy_shutdown_and_discard() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for discard in [false, true] {
            let home = TempDir::new().unwrap();
            let config = test_config(home.path());
            let state = codex_state::StateRuntime::init(
                config.sqlite.clone(),
                config.default_model_provider_id.clone(),
            )
            .await
            .unwrap();
            let gate = Arc::new(WriterMetadataGate::default());
            let store = Arc::new(GatedWriterStore {
                inner: LocalThreadStore::new(config, Some(state.clone())),
                gate: gate.clone(),
            });
            let thread = ThreadId::default();
            let mut params = create_thread_params(thread);
            params.history_mode = ThreadHistoryMode::Paginated;
            let live = LiveThread::create_owned(store.clone(), params)
                .await
                .unwrap();
            live.append_items(&[owned_user_message(
                ThreadHistoryMode::Paginated,
                thread,
                "durable before legacy finish",
            )])
            .await
            .unwrap();
            live.persist(PersistContext::Standard).await.unwrap();
            let path = store.inner.live_rollout_path(thread).await.unwrap();
            let owner = store
                .inner
                .live_recorders
                .lock()
                .await
                .get(&thread)
                .unwrap()
                .owner
                .clone();
            gate.armed.store(true, Ordering::SeqCst);
            let writer = live.clone();
            let write = tokio::spawn(async move {
                writer
                    .update_metadata(
                        ThreadMetadataPatch {
                            preview: Some("before legacy finish".into()),
                            ..Default::default()
                        },
                        /*include_archived*/ true,
                    )
                    .await
            });
            gate.entered.notified().await;
            let mut finish = Box::pin(async {
                if discard {
                    store.inner.discard_thread(thread).await
                } else {
                    store.inner.shutdown_thread(thread).await
                }
            });
            poll_writer_blocked(finish.as_mut(), &owner).await;
            gate.release.notify_one();
            write.await.unwrap().unwrap();
            finish.await.unwrap();
            assert!(path.exists());
            assert!(live.flush().await.is_err());
            let (replacement, history) = LiveThread::resume_owned(
                store.clone(),
                ThreadHistoryMode::Paginated,
                ResumeThreadParams {
                    thread_id: thread,
                    rollout_path: Some(path),
                    history: None,
                    history_revision: None,
                    include_archived: true,
                    metadata: thread_metadata(),
                },
                /*preparation*/ None,
            )
            .await
            .unwrap();
            assert!(
                serde_json::to_string(&history)
                    .unwrap()
                    .contains("durable before legacy finish")
            );
            replacement
                .append_items(&[owned_user_message(
                    ThreadHistoryMode::Paginated,
                    thread,
                    "after legacy finish",
                )])
                .await
                .unwrap();
            replacement.flush().await.unwrap();
            replacement.shutdown().await.unwrap();
        }
    })
    .await
    .expect("bounded actual legacy finish after admitted write");
}

#[tokio::test]
async fn owned_delete_barrier_detects_intentional_lease_bypass() {
    use futures::FutureExt;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let home = TempDir::new().unwrap();
        let config = test_config(home.path());
        let state = codex_state::StateRuntime::init(
            config.sqlite.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let gate = Arc::new(WriterMetadataGate::default());
        let store = Arc::new(GatedWriterStore {
            inner: LocalThreadStore::new(config, Some(state.clone())),
            gate: gate.clone(),
        });
        let thread = ThreadId::default();
        let mut params = create_thread_params(thread);
        params.history_mode = ThreadHistoryMode::Paginated;
        let live = LiveThread::create_owned(store.clone(), params)
            .await
            .unwrap();
        live.persist(PersistContext::Standard).await.unwrap();
        let path = store.inner.live_rollout_path(thread).await.unwrap();
        let owner = store
            .inner
            .live_recorders
            .lock()
            .await
            .get(&thread)
            .unwrap()
            .owner
            .clone();
        let observed = owner.watch_lease_wait();
        gate.armed.store(true, Ordering::SeqCst);
        let writer = live.clone();
        let write = tokio::spawn(async move {
            writer
                .update_metadata(
                    ThreadMetadataPatch {
                        preview: Some("held admitted metadata".into()),
                        ..Default::default()
                    },
                    /*include_archived*/ true,
                )
                .await
        });
        gate.entered.notified().await;
        owner.bypass_lifecycle_for_test();
        // Same real delete path and spawned I/O; ONLY owner admission is broken.
        // It actually deletes the row/file BEFORE releasing the admitted write.
        store
            .inner
            .delete_thread(DeleteThreadParams { thread_id: thread })
            .await
            .unwrap();
        assert!(
            !path.exists(),
            "negative must reach actual destructive effect while gate held"
        );
        assert!(state.get_thread(thread).await.unwrap().is_none());
        assert!(
            observed.notified().now_or_never().is_none(),
            "bypass must not impersonate contested lease admission"
        );
        gate.release.notify_one();
        let _actual_stale_result = write.await.unwrap();
    })
    .await
    .expect("bounded real lease-bypass negative sensitivity");
}

#[tokio::test]
async fn owned_local_failed_discard_rejects_legacy_release_and_changed_intent() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    let outsider = Arc::new(LocalThreadStore::new(config.clone(), Some(state.clone())));
    let store = Arc::new(LocalThreadStore::new(config, Some(state.clone())));
    let thread = ThreadId::default();
    let live = LiveThread::create_owned(store.clone(), create_thread_params(thread))
        .await
        .unwrap();
    let mut guard = LiveThreadInitGuard::new(Some(live.clone()));
    live.append_items(&[user_message_item("provisional retained on conflict")])
        .await
        .unwrap();
    live.persist(PersistContext::Standard).await.unwrap();
    let path = store.live_rollout_path(thread).await.unwrap();
    let (owner, os_lock) = {
        let entries = store.live_recorders.lock().await;
        let entry = entries.get(&thread).unwrap();
        (entry.owner.clone(), Arc::downgrade(&entry.writer_lock))
    };
    let first = crate::NativeAuthorityEpoch {
        version: crate::NATIVE_AUTHORITY_VERSION,
        thread_id: thread,
        root_thread_id: thread,
        parent_thread_id: None,
        generation: uuid::Uuid::now_v7(),
    };
    live.begin_native_authority_epoch(first.clone())
        .await
        .unwrap();
    let mut conflicting = first.clone();
    conflicting.generation = uuid::Uuid::now_v7();
    state
        .begin_native_authority_epoch(&conflicting)
        .await
        .unwrap();
    assert!(guard.discard().await.is_err());
    assert!(store.shutdown_thread(thread).await.is_err());
    assert!(store.discard_thread(thread).await.is_err());
    assert!(
        store
            .finish_thread_owned(thread, owner.clone(), false)
            .await
            .is_err()
    );
    {
        let entries = store.live_recorders.lock().await;
        let entry = entries.get(&thread).unwrap();
        assert!(entry.owner.matches(&owner));
        assert!(std::sync::Weak::ptr_eq(
            &Arc::downgrade(&entry.writer_lock),
            &os_lock
        ));
        assert_eq!(entry.authority_generation, Some(first.generation));
        assert!(entry.stopped);
    }
    assert_eq!(
        state
            .read_native_authority_epoch(thread)
            .await
            .unwrap()
            .unwrap()
            .generation,
        conflicting.generation
    );
    assert!(path.exists());
    assert!(
        LiveThread::resume_owned(
            outsider,
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id: thread,
                rollout_path: Some(path.clone()),
                history: None,
                history_revision: None,
                include_archived: true,
                metadata: thread_metadata(),
            },
            /*preparation*/ None
        )
        .await
        .is_err(),
        "independent Local store must still encounter the OS writer lock"
    );
    state.begin_native_authority_epoch(&first).await.unwrap();
    guard.discard().await.unwrap();
    assert!(!path.exists());
    assert!(state.get_thread(thread).await.unwrap().is_none());
    assert!(
        os_lock.upgrade().is_none(),
        "exact cleanup releases retained OS ownership"
    );
    let replacement = LiveThread::create_owned(store.clone(), create_thread_params(thread))
        .await
        .unwrap();
    replacement
        .append_items(&[user_message_item("usable after exact retry")])
        .await
        .unwrap();
    replacement.shutdown().await.unwrap();
}
