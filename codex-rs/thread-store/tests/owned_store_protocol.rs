//! A genuinely independent store using ONLY the public persistence protocol.
//! No Local/InMemory forwarding, private module access or product test hooks.
use codex_protocol::ThreadId;
use codex_protocol::protocol::{
    EventMsg, SessionSource, ThreadHistoryMode, ThreadMemoryMode, UserMessageEvent,
};
use codex_rollout::RolloutItem;
use codex_thread_store::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
struct Entry {
    owner: Option<ThreadWriterOwner>,
    history: Vec<RolloutItem>,
    preview: Option<String>,
}
#[derive(Clone, Default)]
struct IndependentStore {
    entries: Arc<Mutex<HashMap<ThreadId, Entry>>>,
    gate: Arc<AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    finish_entered: Arc<Notify>,
    bypass_finish: Arc<AtomicBool>,
}
fn conflict() -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: "independent writer occupied".into(),
    }
}
macro_rules! unused_operation {
    ($(fn $name:ident($arg:ident: $ty:ty) -> $out:ty;)*) => {
        $(fn $name(&self, $arg: $ty) -> ThreadStoreFuture<'_, $out> {
            let _ = $arg;
            Box::pin(async { Err(ThreadStoreError::Unsupported { operation: stringify!($name) }) })
        })*
    }
}
impl ThreadStore for IndependentStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    unused_operation! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
    }
    fn create_thread_owned(
        &self,
        params: CreateThreadParams,
    ) -> ThreadStoreFuture<'_, ThreadWriterOwner> {
        Box::pin(async move {
            let mut entries = self.entries.lock().await;
            if entries.contains_key(&params.thread_id) {
                return Err(conflict());
            }
            let owner = ThreadWriterOwner::new();
            entries.insert(
                params.thread_id,
                Entry {
                    owner: Some(owner.clone()),
                    ..Default::default()
                },
            );
            Ok(owner)
        })
    }
    fn resume_thread_owned(
        &self,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, ThreadWriterOwner)> {
        Box::pin(async move {
            assert!(preparation.is_none());
            let mut entries = self.entries.lock().await;
            let entry =
                entries
                    .get_mut(&params.thread_id)
                    .ok_or(ThreadStoreError::ThreadNotFound {
                        thread_id: params.thread_id,
                    })?;
            if entry.owner.is_some() {
                return Err(conflict());
            }
            let owner = ThreadWriterOwner::new();
            entry.owner = Some(owner.clone());
            Ok((Arc::new(entry.history.clone()), owner))
        })
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: ThreadWriterOwner,
        discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        let store = self.clone();
        Box::pin(async move {
            store.finish_entered.notify_one();
            if store.bypass_finish.load(Ordering::SeqCst) {
                // Intentionally broken external-store variant: exact matching
                // alone fails to drain a LiveThread composite. Sensitivity only.
                let mut entries = store.entries.lock().await;
                let entry = entries.get_mut(&thread).unwrap();
                if entry
                    .owner
                    .as_ref()
                    .is_some_and(|current| current.matches(&owner))
                {
                    entry.owner = None;
                }
                return Ok(());
            }
            let operation = owner.retirement(discard).await?;
            let marker = operation.retirement_marker()?;
            operation
                .run(async move {
                    let mut entries = store.entries.lock().await;
                    let entry = entries.get_mut(&thread).unwrap();
                    if !entry
                        .owner
                        .as_ref()
                        .is_some_and(|current| current.matches(&owner))
                    {
                        return Ok(());
                    }
                    // All actual cleanup is complete under the store's own lock.
                    marker.retire()?;
                    entry.owner = None;
                    Ok(())
                })
                .await
        })
    }
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if self.gate.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.entries
                .lock()
                .await
                .get_mut(&params.thread_id)
                .unwrap()
                .history
                .extend(params.items);
            Ok(())
        })
    }
    fn persist_thread(&self, _: ThreadId, _: PersistContext) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn flush_thread(&self, _: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            if let Some(preview) = params.patch.preview {
                self.entries
                    .lock()
                    .await
                    .get_mut(&params.thread_id)
                    .unwrap()
                    .preview = Some(preview);
            }
            Ok(None)
        })
    }
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(async move {
            Ok(StoredThreadHistory {
                thread_id: params.thread_id,
                items: self.entries.lock().await[&params.thread_id].history.clone(),
                revision: None,
            })
        })
    }
    fn shutdown_thread(&self, thread: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(self.legacy_release(thread, false))
    }
    fn discard_thread(&self, thread: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(self.legacy_release(thread, true))
    }
}
impl IndependentStore {
    async fn legacy_release(&self, thread: ThreadId, discard: bool) -> ThreadStoreResult<()> {
        loop {
            let owner = self.entries.lock().await[&thread]
                .owner
                .clone()
                .ok_or_else(conflict)?;
            // Never await admission while holding independent state.
            let operation = owner.legacy_retirement(discard).await?;
            let marker = operation.retirement_marker()?;
            let store = self.clone();
            let removed = operation
                .run(async move {
                    let mut entries = store.entries.lock().await;
                    let entry = entries.get_mut(&thread).unwrap();
                    if !entry
                        .owner
                        .as_ref()
                        .is_some_and(|current| current.matches(&owner))
                    {
                        return Ok(false);
                    }
                    marker.retire()?;
                    entry.owner = None;
                    Ok(true)
                })
                .await?;
            if removed {
                return Ok(());
            }
        }
    }
}
fn params(thread: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread.into(),
        thread_id: thread,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "independent owned protocol".into(),
        base_instructions: Default::default(),
        dynamic_tools: vec![],
        selected_capability_roots: vec![],
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test".into(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}
fn message(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: text.into(),
        ..Default::default()
    }))
}
async fn replacement(store: Arc<IndependentStore>, thread: ThreadId) -> LiveThread {
    LiveThread::resume_owned(
        store,
        ThreadHistoryMode::Legacy,
        ResumeThreadParams {
            thread_id: thread,
            rollout_path: None,
            history: None,
            history_revision: None,
            include_archived: true,
            metadata: params(thread).metadata,
        },
        None,
    )
    .await
    .unwrap()
    .0
}
async fn scenario(bypass: bool) {
    let store = Arc::new(IndependentStore::default());
    let thread = ThreadId::default();
    let old = LiveThread::create_owned(store.clone(), params(thread))
        .await
        .unwrap();
    old.persist(PersistContext::Standard).await.unwrap();
    let owner = store.entries.lock().await[&thread].owner.clone().unwrap();
    store.gate.store(true, Ordering::SeqCst);
    let writer = old.clone();
    let write =
        tokio::spawn(async move { writer.append_items(&[message("old admitted write")]).await });
    store.entered.notified().await;
    // This is the public retirement admission future itself. Its only first
    // suspension is the contested owner mutex: no task spawn or storage I/O.
    let mut admission = Box::pin(owner.retirement(false));
    std::future::poll_fn(|cx| match admission.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(_) => panic!("public retirement bypassed an admitted mutation"),
    })
    .await;
    drop(admission);
    store.bypass_finish.store(bypass, Ordering::SeqCst);
    let finish_store = store.clone();
    let finish_owner = owner.clone();
    let finish = tokio::spawn(async move {
        finish_store
            .finish_thread_owned(thread, finish_owner, false)
            .await
    });
    store.finish_entered.notified().await;
    if bypass {
        finish.await.unwrap().unwrap();
        let current = replacement(store.clone(), thread).await;
        let before = store.entries.lock().await[&thread].history.len();
        store.release.notify_one();
        write.await.unwrap().unwrap();
        assert!(
            store.entries.lock().await[&thread].history.len() > before,
            "negative must actually corrupt replacement through late old ID write"
        );
        store.bypass_finish.store(false, Ordering::SeqCst);
        current.shutdown().await.unwrap();
    } else {
        assert!(
            store.entries.lock().await[&thread]
                .owner
                .as_ref()
                .unwrap()
                .matches(&owner)
        );
        write.abort();
        assert!(write.await.unwrap_err().is_cancelled());
        store.release.notify_one();
        finish.await.unwrap().unwrap();
        let current = replacement(store.clone(), thread).await;
        let before = serde_json::to_value(&store.entries.lock().await[&thread].history).unwrap();
        assert!(
            old.append_items(&[message("stale after retire")])
                .await
                .is_err()
        );
        assert!(old.persist(PersistContext::Standard).await.is_err());
        assert!(old.flush().await.is_err());
        assert_eq!(
            serde_json::to_value(&store.entries.lock().await[&thread].history).unwrap(),
            before
        );
        current
            .append_items(&[message("current usable")])
            .await
            .unwrap();
        current.flush().await.unwrap();
        current.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn independent_public_store_drains_cancelled_mutation_and_rejects_stale_writer() {
    tokio::time::timeout(std::time::Duration::from_secs(30), scenario(false))
        .await
        .unwrap();
}
#[tokio::test]
async fn independent_public_store_fixture_detects_exact_match_without_lease() {
    tokio::time::timeout(std::time::Duration::from_secs(30), scenario(true))
        .await
        .unwrap();
}
#[tokio::test]
async fn public_retirement_marker_cannot_complete_outside_its_running_task() {
    let owner = ThreadWriterOwner::new();
    let operation = owner.retirement(false).await.unwrap();
    let marker = operation.retirement_marker().unwrap();
    assert!(marker.retire().is_err());
    let escaped = operation.run(async move { Ok(marker) }).await.unwrap();
    assert!(escaped.retire().is_err());
    assert!(owner.operation().await.is_err()); // cleanup never certified Retired
    assert!(owner.retirement(true).await.is_err());
    assert!(owner.legacy_retirement(false).await.is_err());
    assert!(owner.legacy_retirement(true).await.is_err());
    let retry = owner.retirement(false).await.unwrap();
    let marker = retry.retirement_marker().unwrap();
    retry.run(async move { marker.retire() }).await.unwrap();
    assert!(owner.operation().await.is_err());
}
