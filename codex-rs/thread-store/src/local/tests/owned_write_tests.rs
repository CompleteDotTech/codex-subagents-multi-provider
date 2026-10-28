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
            self.inner.$name($arg)
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
        inner: LocalThreadStore::new(config, Some(state)),
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
        None,
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
            true,
        )
        .await
        .expect("distinguishable actual replacement metadata");
    replacement.flush().await.unwrap();
    let before = replacement.read_thread(true, false).await.unwrap();
    assert_eq!(before.preview, "current replacement preview");
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
    let after = replacement.read_thread(true, false).await.unwrap();
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
