use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutPersistenceTelemetry;
use codex_rollout::measure_and_filter_rollout_items;
use codex_rollout::persisted_rollout_items;
use tokio::sync::Mutex;
use tracing::warn;

use crate::AppendThreadItemsParams;
use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::PersistContext;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::ThreadStoreResumePreparation;
use crate::UpdateThreadMetadataParams;
use crate::thread_metadata_sync::ThreadMetadataSync;

/// Handle for an active thread's persistence lifecycle.
///
/// `LiveThread` keeps lifecycle decisions with the caller while delegating storage details to
/// [`ThreadStore`]. Local stores may use a rollout file internally and remote stores may use a
/// service, but session code should only need this handle for the active thread.
#[derive(Clone)]
pub struct LiveThread {
    thread_id: ThreadId,
    history_mode: ThreadHistoryMode,
    thread_store: Arc<dyn ThreadStore>,
    metadata_sync: Arc<Mutex<ThreadMetadataSync>>,
    persistence_telemetry: RolloutPersistenceTelemetry,
    owner: Option<crate::ThreadWriterOwner>,
    fresh: bool,
    provisional: Arc<std::sync::atomic::AtomicBool>,
}

/// Owns persistence acquisition and its live thread while initialization is still fallible.
///
/// If initialization returns early after persistence has been opened, dropping this guard discards
/// the live writer without forcing lazy in-memory state to become durable. Call [`commit`] once the
/// session owns the live thread for normal operation. Cancellation leaves an in-flight acquisition
/// owned by the guard: cleanup waits for it to finish before discarding any acquired writer.
#[derive(Default)]
pub struct LiveThreadInitGuard {
    live_thread: Option<LiveThread>,
    acquiring: Option<ThreadStoreFuture<'static, LiveThread>>,
}

impl LiveThreadInitGuard {
    pub fn new(live_thread: Option<LiveThread>) -> Self {
        if let Some(thread) = &live_thread {
            thread
                .provisional
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Self {
            live_thread,
            acquiring: None,
        }
    }

    /// Retains the operation even if the caller stops waiting. Store operations may install a
    /// writer before returning or delegate to another runtime, so they cannot simply be dropped.
    pub async fn acquire(
        &mut self,
        acquisition: impl Future<Output = ThreadStoreResult<LiveThread>> + Send + 'static,
    ) -> ThreadStoreResult<LiveThread> {
        assert!(self.live_thread.is_none() && self.acquiring.is_none());
        self.acquiring = Some(Box::pin(acquisition));
        self.finish_acquisition().await?;
        self.live_thread
            .clone()
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "persistence acquisition returned no live thread".to_owned(),
            })
    }

    async fn finish_acquisition(&mut self) -> ThreadStoreResult<()> {
        if let Some(acquiring) = self.acquiring.as_mut() {
            let result = acquiring.await;
            self.acquiring = None;
            let thread = result?;
            thread
                .provisional
                .store(true, std::sync::atomic::Ordering::Release);
            self.live_thread = Some(thread);
        }
        Ok(())
    }

    pub fn as_ref(&self) -> Option<&LiveThread> {
        self.live_thread.as_ref()
    }

    pub fn commit(&mut self) {
        if let Some(thread) = &self.live_thread {
            thread
                .provisional
                .store(false, std::sync::atomic::Ordering::Release);
        }
        self.live_thread = None;
    }

    pub async fn discard(&mut self) -> ThreadStoreResult<()> {
        self.finish_acquisition().await?;
        let Some(live_thread) = self.live_thread.as_ref() else {
            return Ok(());
        };
        live_thread.discard().await?;
        self.live_thread = None;
        Ok(())
    }
}

impl Drop for LiveThreadInitGuard {
    fn drop(&mut self) {
        if self.live_thread.is_none() && self.acquiring.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("failed to discard thread persistence for failed session init: no Tokio runtime");
            return;
        };
        let acquiring = self.acquiring.take();
        let live_thread = self.live_thread.take();
        handle.spawn(async move {
            let live_thread = match acquiring {
                Some(acquiring) => acquiring.await.ok(),
                None => live_thread,
            };
            if let Some(live_thread) = live_thread {
                // An acquisition completed by this cancellation owner has not
                // crossed the registration commit, just like finish_acquisition.
                live_thread
                    .provisional
                    .store(true, std::sync::atomic::Ordering::Release);
                if let Err(err) = live_thread.discard().await {
                    warn!("failed to discard thread persistence for failed session init: {err}");
                }
            }
        });
    }
}

impl LiveThread {
    async fn mutate<T, F, Fut>(&self, work: F) -> ThreadStoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Fut,
        Fut: Future<Output = ThreadStoreResult<T>> + Send + 'static,
    {
        let Some(owner) = &self.owner else {
            return work(self.clone()).await;
        };
        let operation = owner.operation().await?;
        crate::store::complete_writer_operation(operation, work(self.clone())).await
    }

    pub async fn append_items(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        let items = raw_items.to_vec();
        self.mutate(move |thread| async move { thread.append_items_inner(&items).await })
            .await
    }

    async fn persist_inherited_items(
        &self,
        raw_items: &[RolloutItem],
    ) -> ThreadStoreResult<Vec<RolloutItem>> {
        let items = raw_items.to_vec();
        self.mutate(move |thread| async move { thread.persist_appended_items(&items).await })
            .await
    }

    pub async fn persist(&self, context: PersistContext) -> ThreadStoreResult<()> {
        self.mutate(move |thread| async move { thread.persist_inner(context).await })
            .await
    }
    pub async fn flush(&self) -> ThreadStoreResult<()> {
        self.mutate(|thread| async move { thread.flush_inner().await })
            .await
    }
    pub async fn update_memory_mode(
        &self,
        mode: ThreadMemoryMode,
        include_archived: bool,
    ) -> ThreadStoreResult<()> {
        self.mutate(move |thread| async move {
            thread
                .update_memory_mode_inner(mode, include_archived)
                .await
        })
        .await
    }
    pub async fn update_metadata(
        &self,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThread> {
        self.mutate(move |thread| async move {
            thread.update_metadata_inner(patch, include_archived).await
        })
        .await
    }

    pub async fn shutdown(&self) -> ThreadStoreResult<()> {
        let Some(owner) = &self.owner else {
            return self.shutdown_inner().await;
        };
        // Wait for admission in the caller. No detached operation can start if
        // that wait is cancelled. Once admitted, own metadata AND final cleanup.
        let operation = owner.operation().await;
        let thread = self.clone();
        tokio::spawn(async move {
            let metadata_result = match operation {
                Ok(operation) => {
                    let metadata_thread = thread.clone();
                    crate::store::complete_writer_operation(operation, async move {
                        if metadata_thread
                            .provisional
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            Ok(())
                        } else {
                            metadata_thread
                                .flush_pending_metadata_update_for_existing_history()
                                .await
                        }
                    })
                    .await
                }
                Err(error) => Err(error),
            };
            thread.finish_shutdown(metadata_result).await
        })
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("owned shutdown task failed: {error}"),
        })?
    }

    pub async fn begin_native_authority_epoch(
        &self,
        epoch: crate::NativeAuthorityEpoch,
    ) -> ThreadStoreResult<()> {
        match &self.owner {
            Some(owner) => {
                self.thread_store
                    .begin_native_authority_epoch_owned(epoch, owner.clone())
                    .await
            }
            None => self.thread_store.begin_native_authority_epoch(epoch).await,
        }
    }

    pub async fn create(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
    ) -> ThreadStoreResult<Self> {
        Self::create_impl(thread_store, params, false).await
    }
    pub async fn create_owned(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
    ) -> ThreadStoreResult<Self> {
        Self::create_impl(thread_store, params, true).await
    }
    async fn create_impl(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
        require_owned: bool,
    ) -> ThreadStoreResult<Self> {
        let thread_id = params.thread_id;
        let history_mode = params.history_mode;
        let metadata_sync = ThreadMetadataSync::for_create(&params).await;
        let owner = if require_owned {
            Some(thread_store.create_thread_owned(params).await?)
        } else {
            thread_store.create_thread(params).await?;
            None
        };
        let provisional = owner
            .as_ref()
            .map(crate::ThreadWriterOwner::provisional)
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        provisional.store(false, std::sync::atomic::Ordering::Release);
        Ok(Self {
            owner,
            fresh: true,
            provisional,
            thread_id,
            history_mode,
            thread_store,
            metadata_sync: Arc::new(Mutex::new(metadata_sync)),
            persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
        })
    }

    /// Create a child thread with inherited model context already durable.
    ///
    /// The boundary belongs in session metadata before the copied prefix is written so history
    /// projection can distinguish inherited context from the child's own records immediately.
    pub async fn create_with_inherited_model_context(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
        inherited_model_context: &[RolloutItem],
        guard: &mut LiveThreadInitGuard,
    ) -> ThreadStoreResult<Self> {
        Self::create_inherited_impl(thread_store, params, inherited_model_context, guard, false)
            .await
    }
    pub async fn create_owned_with_inherited_model_context(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
        inherited_model_context: &[RolloutItem],
        guard: &mut LiveThreadInitGuard,
    ) -> ThreadStoreResult<Self> {
        Self::create_inherited_impl(thread_store, params, inherited_model_context, guard, true)
            .await
    }
    async fn create_inherited_impl(
        thread_store: Arc<dyn ThreadStore>,
        mut params: CreateThreadParams,
        inherited_model_context: &[RolloutItem],
        guard: &mut LiveThreadInitGuard,
        require_owned: bool,
    ) -> ThreadStoreResult<Self> {
        let persisted_prefix_item_count =
            persisted_rollout_items(inherited_model_context, params.history_mode).len();
        params.subagent_history_start_ordinal = Some(
            u64::try_from(persisted_prefix_item_count)
                .map_err(|_| ThreadStoreError::Internal {
                    message: "inherited model context is too large".to_string(),
                })?
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "inherited model context is too large".to_string(),
                })?,
        );
        let live_thread = guard
            .acquire(Self::create_impl(thread_store, params, require_owned))
            .await?;
        live_thread
            .persist_inherited_items(inherited_model_context)
            .await?;
        Ok(live_thread)
    }

    /// Reopens persistence and returns the replay history selected under writer ownership.
    pub async fn resume(
        thread_store: Arc<dyn ThreadStore>,
        history_mode: ThreadHistoryMode,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreResult<(Self, Arc<Vec<RolloutItem>>)> {
        Self::resume_impl(thread_store, history_mode, params, preparation, false).await
    }
    pub async fn resume_owned(
        thread_store: Arc<dyn ThreadStore>,
        history_mode: ThreadHistoryMode,
        params: ResumeThreadParams,
        preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreResult<(Self, Arc<Vec<RolloutItem>>)> {
        Self::resume_impl(thread_store, history_mode, params, preparation, true).await
    }
    async fn resume_impl(
        thread_store: Arc<dyn ThreadStore>,
        history_mode: ThreadHistoryMode,
        mut params: ResumeThreadParams,
        resume_preparation: Option<ThreadStoreResumePreparation>,
        require_owned: bool,
    ) -> ThreadStoreResult<(Self, Arc<Vec<RolloutItem>>)> {
        let thread_id = params.thread_id;
        let metadata = if history_mode == ThreadHistoryMode::Paginated
            && let Some(local_store) = thread_store.as_any().downcast_ref::<LocalThreadStore>()
            && let Some(state_db) = local_store.state_db().await
        {
            state_db
                .get_thread(thread_id)
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to read thread metadata for {thread_id}: {err}"),
                })?
        } else {
            None
        };
        let (history, owner) = if require_owned {
            let (history, owner) = thread_store
                .resume_thread_owned(params.clone(), resume_preparation)
                .await?;
            (history, Some(owner))
        } else {
            let history = match resume_preparation {
                Some(preparation) => {
                    thread_store
                        .resume_thread_with_preparation(params.clone(), preparation)
                        .await?
                }
                None => thread_store.resume_thread(params.clone()).await?,
            };
            (history, None)
        };
        let provisional = owner
            .as_ref()
            .map(crate::ThreadWriterOwner::provisional)
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        provisional.store(false, std::sync::atomic::Ordering::Release);
        params.history = Some(Arc::clone(&history));
        let metadata_sync = ThreadMetadataSync::for_resume(&params, metadata.as_ref());
        Ok((
            Self {
                owner,
                fresh: false,
                provisional,
                thread_id,
                history_mode,
                thread_store,
                metadata_sync: Arc::new(Mutex::new(metadata_sync)),
                persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
            },
            history,
        ))
    }

    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(item_count = raw_items.len())
    )]
    async fn append_items_inner(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        let items = self.persist_appended_items(raw_items).await?;
        if items.is_empty() {
            return Ok(());
        }
        let update = self
            .metadata_sync
            .lock()
            .await
            .observe_appended_items(items.as_slice());
        if let Some(update) = update {
            self.thread_store
                .record_thread_metadata(UpdateThreadMetadataParams {
                    thread_id: self.thread_id,
                    patch: update.patch.clone(),
                    include_archived: true,
                })
                .await?;
            self.metadata_sync
                .lock()
                .await
                .mark_pending_update_applied(&update);
        }
        Ok(())
    }

    async fn persist_appended_items(
        &self,
        raw_items: &[RolloutItem],
    ) -> ThreadStoreResult<Vec<RolloutItem>> {
        // Empty appends are intentionally ignored rather than represented as zero-sized batches.
        if raw_items.is_empty() {
            return Ok(Vec::new());
        }
        let (items, measurement) = if self.persistence_telemetry.is_enabled() {
            let (items, measurement) =
                measure_and_filter_rollout_items(raw_items, self.history_mode);
            (items, Some(measurement))
        } else {
            (persisted_rollout_items(raw_items, self.history_mode), None)
        };
        self.thread_store
            .append_items(AppendThreadItemsParams {
                thread_id: self.thread_id,
                items: raw_items.to_vec(),
            })
            .await?;
        if let Some(measurement) = measurement.as_ref() {
            self.persistence_telemetry
                .record_batch(raw_items, measurement);
        }
        Ok(items)
    }

    async fn persist_inner(&self, context: PersistContext) -> ThreadStoreResult<()> {
        if context.allows_background_persistence() {
            let update = self
                .metadata_sync
                .lock()
                .await
                .take_pending_update_for_existing_history();
            self.apply_pending_metadata_update(update, context).await?;
        }
        self.thread_store
            .persist_thread(self.thread_id, context)
            .await?;
        let update = self.metadata_sync.lock().await.take_pending_update();
        self.apply_pending_metadata_update(update, context).await
    }

    async fn flush_inner(&self) -> ThreadStoreResult<()> {
        self.thread_store.flush_thread(self.thread_id).await?;
        self.flush_pending_metadata_update_for_existing_history()
            .await
    }

    async fn shutdown_inner(&self) -> ThreadStoreResult<()> {
        let metadata_result = if self.provisional.load(std::sync::atomic::Ordering::Acquire) {
            Ok(())
        } else {
            self.flush_pending_metadata_update_for_existing_history()
                .await
        };
        self.finish_shutdown(metadata_result).await
    }

    async fn finish_shutdown(
        &self,
        metadata_result: ThreadStoreResult<()>,
    ) -> ThreadStoreResult<()> {
        let shutdown_result = match &self.owner {
            Some(owner) => {
                self.thread_store
                    .finish_thread_owned(
                        self.thread_id,
                        owner.clone(),
                        self.fresh && self.provisional.load(std::sync::atomic::Ordering::Acquire),
                    )
                    .await
            }
            None => self.thread_store.shutdown_thread(self.thread_id).await,
        };
        match (metadata_result, shutdown_result) {
            (Err(metadata_error), Err(shutdown_error)) => Err(ThreadStoreError::Internal {
                message: format!(
                    "thread metadata update failed: {metadata_error}; thread shutdown failed: {shutdown_error}"
                ),
            }),
            (Err(metadata_error), Ok(())) => Err(metadata_error),
            (Ok(()), result) => result,
        }
    }

    pub async fn discard(&self) -> ThreadStoreResult<()> {
        match &self.owner {
            Some(owner) => {
                self.thread_store
                    .finish_thread_owned(self.thread_id, owner.clone(), true)
                    .await
            }
            None => self.thread_store.discard_thread(self.thread_id).await,
        }
    }

    pub async fn load_history(
        &self,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        self.thread_store
            .load_history(LoadThreadHistoryParams {
                thread_id: self.thread_id,
                include_archived,
            })
            .await
    }

    pub async fn read_thread(
        &self,
        include_archived: bool,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        self.thread_store
            .read_thread(ReadThreadParams {
                thread_id: self.thread_id,
                include_archived,
                include_history,
            })
            .await
    }

    async fn update_memory_mode_inner(
        &self,
        mode: ThreadMemoryMode,
        include_archived: bool,
    ) -> ThreadStoreResult<()> {
        self.flush_pending_metadata_update().await?;
        self.thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch: ThreadMetadataPatch {
                    memory_mode: Some(mode),
                    ..Default::default()
                },
                include_archived,
            })
            .await?;
        Ok(())
    }

    /// Updates metadata while preserving this API's materialized-thread contract.
    ///
    /// Stores may successfully return no thread for a no-op update, so this reads the thread as a
    /// fallback in that case.
    async fn update_metadata_inner(
        &self,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThread> {
        self.flush_pending_metadata_update().await?;
        let updated = self
            .thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch,
                include_archived,
            })
            .await?;
        match updated {
            Some(thread) => Ok(thread),
            None => {
                self.read_thread(include_archived, /*include_history*/ false)
                    .await
            }
        }
    }

    /// Returns the live local rollout path for legacy local-only callers.
    ///
    /// Remote stores do not expose rollout files, so they return `Ok(None)`.
    pub async fn local_rollout_path(&self) -> ThreadStoreResult<Option<PathBuf>> {
        let Some(local_store) = self
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        else {
            return Ok(None);
        };
        local_store
            .live_rollout_path(self.thread_id)
            .await
            .map(Some)
    }

    /// Selects a local feedback input only after its writer has materialized it.
    pub async fn feedback_rollout_path(&self) -> ThreadStoreResult<Option<PathBuf>> {
        let Some(local_store) = self
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        else {
            return Ok(None);
        };
        local_store.feedback_rollout_path(self.thread_id).await
    }

    async fn flush_pending_metadata_update(&self) -> ThreadStoreResult<()> {
        let update = self.metadata_sync.lock().await.take_pending_update();
        self.apply_pending_metadata_update(update, PersistContext::Standard)
            .await
    }

    async fn flush_pending_metadata_update_for_existing_history(&self) -> ThreadStoreResult<()> {
        let update = self
            .metadata_sync
            .lock()
            .await
            .take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(update, PersistContext::Standard)
            .await
    }

    async fn apply_pending_metadata_update(
        &self,
        update: Option<crate::thread_metadata_sync::PendingThreadMetadataPatch>,
        context: PersistContext,
    ) -> ThreadStoreResult<()> {
        let Some(update) = update else {
            return Ok(());
        };
        let params = UpdateThreadMetadataParams {
            thread_id: self.thread_id,
            patch: update.patch.clone(),
            include_archived: true,
        };
        if context == PersistContext::Standard {
            self.thread_store.update_thread_metadata(params).await?;
        } else {
            self.thread_store.record_thread_metadata(params).await?;
        }
        self.metadata_sync
            .lock()
            .await
            .mark_pending_update_applied(&update);
        Ok(())
    }
}
