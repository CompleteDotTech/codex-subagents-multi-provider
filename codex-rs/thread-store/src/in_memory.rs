use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::into_persisted_rollout_items;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LoadThreadHistoryParams;
use crate::MoveThreadToSectionParams;
use crate::PersistContext;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;

static IN_MEMORY_THREAD_STORES: OnceLock<Mutex<HashMap<String, Arc<InMemoryThreadStore>>>> =
    OnceLock::new();

fn stores() -> &'static Mutex<HashMap<String, Arc<InMemoryThreadStore>>> {
    IN_MEMORY_THREAD_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ItemSortKey;
    use crate::ListItemsParams;
    use crate::ListTurnsParams;
    use crate::SortDirection;
    use crate::StoredTurnItemsView;
    use crate::ThreadPersistenceMetadata;
    use crate::ThreadSortKey;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::protocol::SessionSource;

    #[tokio::test]
    async fn deletion_cleans_associated_sqlite_and_shared_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        use codex_utils_absolute_path::test_support::PathExt;
        use pretty_assertions::assert_eq;

        let home = tempfile::TempDir::new()?;
        let state_db = codex_state::StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            "test".to_string(),
        )
        .await?;
        let shared = InMemoryThreadStore::default();
        let store = shared.with_state_db(Some(state_db.clone()));
        let thread_id = ThreadId::new();
        shared
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await?;
        let metadata = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            home.path().join("thread.jsonl"),
            Utc::now(),
            SessionSource::Cli,
        )
        .build("test");
        state_db.upsert_thread(&metadata).await?;

        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await?;

        assert_eq!(state_db.get_thread(thread_id).await?, None);
        assert!(!shared.state.lock().await.histories.contains_key(&thread_id));
        Ok(())
    }

    #[tokio::test]
    async fn default_turn_pagination_methods_return_unsupported() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();

        let turns_err = store
            .list_turns(ListTurnsParams {
                thread_id,
                include_archived: true,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
                items_view: StoredTurnItemsView::Summary,
            })
            .await
            .expect_err("default list_turns should be unsupported");
        assert!(matches!(
            turns_err,
            ThreadStoreError::Unsupported {
                operation: "list_turns"
            }
        ));

        let items_err = store
            .list_items(ListItemsParams {
                item_ids: None,
                thread_id,
                turn_id: None,
                include_archived: true,
                position: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
                sort_key: ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
            })
            .await
            .expect_err("default list_items should be unsupported");
        assert!(matches!(
            items_err,
            ThreadStoreError::Unsupported {
                operation: "list_items"
            }
        ));
    }

    #[tokio::test]
    async fn list_threads_filters_by_spawn_relationship() {
        let store = InMemoryThreadStore::default();
        let parent_thread_id = ThreadId::default();
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
        let unrelated_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
        let grandchild_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid thread id");

        for (thread_id, parent_thread_id) in [
            (child_thread_id, Some(parent_thread_id)),
            (unrelated_thread_id, None),
            (grandchild_thread_id, Some(child_thread_id)),
        ] {
            store
                .create_thread(CreateThreadParams {
                    creator_user_id: None,
                    creator_account_id: None,
                    session_id: thread_id.into(),
                    thread_id,
                    extra_config: None,
                    forked_from_id: None,
                    parent_thread_id,
                    source: SessionSource::Exec,
                    thread_source: None,
                    originator: "test_originator".to_string(),
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
                        model_provider: "test-provider".to_string(),
                        memory_mode: ThreadMemoryMode::Enabled,
                    },
                })
                .await
                .expect("create thread");
        }

        store
            .move_thread_to_section(MoveThreadToSectionParams {
                thread_id: grandchild_thread_id,
                section: Some(codex_state::PINNED_THREAD_SECTION_ID.to_string()),
                before_thread_id: None,
            })
            .await
            .expect("pin grandchild thread");

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: None,
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list child threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![child_thread_id]
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: None,
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<HashSet<_>>(),
            HashSet::from([child_thread_id, grandchild_thread_id])
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: Some(Some(codex_state::PINNED_THREAD_SECTION_ID.to_string())),
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list pinned descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![grandchild_thread_id]
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: Some(None),
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list unsectioned descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![child_thread_id]
        );
    }

    #[tokio::test]
    async fn paginated_threads_allow_metadata_reads_and_resume_but_reject_legacy_history_paths() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        let rollout_path = PathBuf::from("/tmp/paginated-thread.jsonl");

        store
            .create_thread(create_thread_params(
                thread_id,
                ThreadHistoryMode::Paginated,
            ))
            .await
            .expect("create paginated thread");
        store
            .resume_thread(ResumeThreadParams {
                history_revision: None,
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            })
            .await
            .expect("register rollout path");

        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        let thread = store
            .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                rollout_path,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata path read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        assert_paginated_threads_unsupported(
            store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                    rollout_path: PathBuf::from("/tmp/paginated-thread.jsonl"),
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history path read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await
                .expect_err("history load should fail"),
        );
        store
            .resume_thread(ResumeThreadParams {
                history_revision: None,
                thread_id,
                rollout_path: None,
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume should succeed");
    }

    #[tokio::test]
    async fn metadata_update_returns_the_materialized_thread() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        store
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await
            .expect("create thread");

        let updated = ThreadStore::update_thread_metadata(
            &store,
            UpdateThreadMetadataParams {
                thread_id,
                patch: ThreadMetadataPatch {
                    name: Some(Some("renamed".to_string())),
                    ..Default::default()
                },
                include_archived: false,
            },
        )
        .await
        .expect("update metadata");
        let updated = updated.expect("in-memory store returns updated thread");
        assert_eq!(updated.thread_id, thread_id);
        assert_eq!(updated.name.as_deref(), Some("renamed"));
    }

    fn create_thread_params(
        thread_id: ThreadId,
        history_mode: ThreadHistoryMode,
    ) -> CreateThreadParams {
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
            originator: "test_originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: thread_metadata(),
        }
    }

    fn thread_metadata() -> ThreadPersistenceMetadata {
        ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        }
    }

    fn assert_paginated_threads_unsupported(err: ThreadStoreError) {
        assert!(matches!(
            err,
            ThreadStoreError::Unsupported {
                operation: "paginated_threads"
            }
        ));
    }
}

fn stores_guard() -> MutexGuard<'static, HashMap<String, Arc<InMemoryThreadStore>>> {
    match stores().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Recorded call counts for [`InMemoryThreadStore`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryThreadStoreCalls {
    pub create_thread: usize,
    pub resume_thread: usize,
    pub append_items: usize,
    pub persist_thread: usize,
    pub flush_thread: usize,
    pub shutdown_thread: usize,
    pub discard_thread: usize,
    pub load_history: usize,
    pub load_latest_model_context: usize,
    pub read_thread: usize,
    pub read_thread_with_history: usize,
    pub read_thread_by_rollout_path: usize,
    pub list_threads: usize,
    pub update_thread_metadata: usize,
    pub archive_thread: usize,
    pub unarchive_thread: usize,
    pub delete_thread: usize,
}

/// In-memory [`ThreadStore`] implementation for tests and debug configs.
///
/// Test and debug configs can select this store by id, letting tests exercise
/// config-driven non-local persistence without requiring the real remote gRPC
/// service.
#[derive(Default)]
pub struct InMemoryThreadStore {
    state: Arc<tokio::sync::Mutex<InMemoryThreadStoreState>>,
    omit_metadata_update_result: Arc<AtomicBool>,
    state_db: Option<codex_rollout::StateDbHandle>,
}

#[derive(Default)]
struct InMemoryThreadStoreState {
    writer_owners: HashMap<ThreadId, crate::ThreadWriterOwner>,
    fresh_writers: std::collections::HashSet<ThreadId>,
    writer_authority_generations: HashMap<ThreadId, uuid::Uuid>,
    calls: InMemoryThreadStoreCalls,
    repeated_list_threads_cursor: Option<String>,
    created_threads: HashMap<ThreadId, CreateThreadParams>,
    histories: HashMap<ThreadId, Vec<RolloutItem>>,
    metadata_updates: HashMap<ThreadId, ThreadMetadataPatch>,
    sections: HashMap<ThreadId, String>,
    section_positions: HashMap<ThreadId, i64>,
    section_entered_at: HashMap<ThreadId, DateTime<Utc>>,
    names: HashMap<ThreadId, Option<String>>,
    rollout_paths: HashMap<PathBuf, ThreadId>,
    native_authority: HashMap<
        ThreadId,
        (
            crate::NativeAuthorityEpoch,
            Option<crate::NativeAuthorityReceipt>,
        ),
    >,
}

impl InMemoryThreadStore {
    #[cfg(test)]
    async fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        self.create_thread_owned_impl(params, /*exclusive*/ false)
            .await
            .map(|_| ())
    }
    #[cfg(test)]
    async fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreResult<Arc<Vec<RolloutItem>>> {
        self.resume_thread_owned_impl(params, /*exclusive*/ false)
            .await
            .map(|(history, _)| history)
    }
    /// Returns the store associated with `id`, creating it if needed.
    pub fn for_id(id: impl Into<String>) -> Arc<Self> {
        let id = id.into();
        let mut stores = stores_guard();
        stores
            .entry(id)
            .or_insert_with(|| Arc::new(Self::default()))
            .clone()
    }

    /// Shares this debug store's thread data while owning cleanup of the caller's SQLite state.
    pub fn with_state_db(&self, state_db: Option<codex_rollout::StateDbHandle>) -> Self {
        Self {
            state: Arc::clone(&self.state),
            omit_metadata_update_result: Arc::clone(&self.omit_metadata_update_result),
            state_db,
        }
    }

    /// Removes a shared in-memory store for `id`.
    pub fn remove_id(id: &str) -> Option<Arc<Self>> {
        stores_guard().remove(id)
    }

    /// Returns the calls observed by this store.
    pub async fn calls(&self) -> InMemoryThreadStoreCalls {
        self.state.lock().await.calls.clone()
    }

    /// Makes every thread-list page return the same cursor for public API error-path tests.
    pub async fn repeat_list_threads_cursor_for_testing(&self, cursor: impl Into<String>) {
        self.state.lock().await.repeated_list_threads_cursor = Some(cursor.into());
    }

    /// Makes metadata updates apply normally while returning no materialized thread.
    pub fn omit_metadata_update_result_for_testing(&self) {
        self.omit_metadata_update_result
            .store(true, Ordering::Relaxed);
    }

    async fn lock_writer_lifecycle(
        &self,
        thread: ThreadId,
    ) -> ThreadStoreResult<(
        Option<crate::store::WriterOperation>,
        tokio::sync::OwnedMutexGuard<InMemoryThreadStoreState>,
    )> {
        loop {
            let state = self.state.clone().lock_owned().await;
            let Some(owner) = state.writer_owners.get(&thread).cloned() else {
                return Ok((None, state));
            };
            drop(state);
            let operation = owner.lifecycle().await?;
            let state = self.state.clone().lock_owned().await;
            if state
                .writer_owners
                .get(&thread)
                .is_some_and(|current| current.matches(&owner))
            {
                return Ok((Some(operation), state));
            }
            drop(state);
            operation.complete();
        }
    }

    async fn create_thread_owned_impl(
        &self,
        params: CreateThreadParams,
        exclusive: bool,
    ) -> ThreadStoreResult<crate::ThreadWriterOwner> {
        // Exclusive acquisition rejects an existing owner without waiting on it.
        if exclusive
            && self
                .state
                .lock()
                .await
                .writer_owners
                .contains_key(&params.thread_id)
        {
            return Err(ThreadStoreError::Conflict {
                message: "thread already has a live owned writer".into(),
            });
        }
        let (retiring, mut state) = self.lock_writer_lifecycle(params.thread_id).await?;
        if exclusive && state.writer_owners.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::Conflict {
                message: "thread already has a live owned writer".into(),
            });
        }
        if exclusive
            && (state.histories.contains_key(&params.thread_id)
                || state.native_authority.contains_key(&params.thread_id))
        {
            return Err(ThreadStoreError::Conflict {
                message: "fresh thread identity already has saved history".into(),
            });
        }
        if let Some(operation) = retiring.as_ref() {
            operation.require_active()?;
        }
        let mut retiring = retiring;
        if let Some(operation) = retiring.as_mut() {
            operation.start_retirement();
        }
        state.calls.create_thread += 1;
        let session_meta = SessionMeta {
            session_id: params.session_id,
            id: params.thread_id,
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            cwd: params.metadata.cwd.clone().unwrap_or_default(),
            runtime_workspace_roots: params
                .runtime_workspace_roots
                .as_ref()
                .map(|roots| roots.iter().map(AbsolutePathBuf::to_path_buf).collect()),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            originator: params.originator.clone(),
            creator_user_id: params.creator_user_id.clone(),
            creator_account_id: params.creator_account_id.clone(),
            source: params.source.clone(),
            thread_source: params.thread_source.clone(),
            model_provider: Some(params.metadata.model_provider.clone()),
            base_instructions: Some(params.base_instructions.clone()),
            dynamic_tools: (!params.dynamic_tools.is_empty()).then(|| params.dynamic_tools.clone()),
            selected_capability_roots: params.selected_capability_roots.clone(),
            memory_mode: matches!(params.metadata.memory_mode, ThreadMemoryMode::Disabled)
                .then_some("disabled".to_string()),
            history_mode: params.history_mode,
            history_base: params.history_base,
            subagent_history_start_ordinal: params.subagent_history_start_ordinal,
            multi_agent_version: params.multi_agent_version,
            context_window: Some(SessionContextWindow::new(params.initial_window_id.clone())),
            ..SessionMeta::default()
        };
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .push(RolloutItem::SessionMeta(Box::new(SessionMetaLine {
                meta: session_meta,
                git: None,
            })));
        let owner = crate::ThreadWriterOwner::new();
        if let Some(operation) = retiring {
            operation.retired();
            operation.complete();
        }
        state.writer_authority_generations.remove(&params.thread_id);
        state.writer_owners.insert(params.thread_id, owner.clone());
        state.fresh_writers.insert(params.thread_id);
        state.created_threads.insert(params.thread_id, params);
        Ok(owner)
    }

    async fn resume_thread_owned_impl(
        &self,
        params: ResumeThreadParams,
        exclusive: bool,
    ) -> ThreadStoreResult<(Arc<Vec<RolloutItem>>, crate::ThreadWriterOwner)> {
        // Exclusive acquisition rejects an existing owner without waiting on it.
        if exclusive
            && self
                .state
                .lock()
                .await
                .writer_owners
                .contains_key(&params.thread_id)
        {
            return Err(ThreadStoreError::Conflict {
                message: "thread already has a live owned writer".into(),
            });
        }
        let (retiring, mut state) = self.lock_writer_lifecycle(params.thread_id).await?;
        if exclusive && state.writer_owners.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::Conflict {
                message: "thread already has a live owned writer".into(),
            });
        }
        if let Some(operation) = retiring.as_ref() {
            operation.require_active()?;
        }
        let mut retiring = retiring;
        if let Some(operation) = retiring.as_mut() {
            operation.start_retirement();
        }
        state.calls.resume_thread += 1;
        if let Some(history) = params.history {
            if matches!(
                history.first(),
                Some(RolloutItem::SessionMeta(meta)) if meta.meta.id == params.thread_id
            ) {
                state
                    .histories
                    .entry(params.thread_id)
                    .or_insert_with(|| Arc::unwrap_or_clone(history));
            } else {
                state
                    .histories
                    .insert(params.thread_id, Arc::unwrap_or_clone(history));
            }
        } else {
            state.histories.entry(params.thread_id).or_default();
        }
        if let Some(rollout_path) = params.rollout_path {
            state.rollout_paths.insert(rollout_path, params.thread_id);
        }
        let owner = crate::ThreadWriterOwner::new();
        if let Some(operation) = retiring {
            operation.retired();
            operation.complete();
        }
        state.writer_authority_generations.remove(&params.thread_id);
        state.writer_owners.insert(params.thread_id, owner.clone());
        state.fresh_writers.remove(&params.thread_id);
        Ok((Arc::new(state.histories[&params.thread_id].clone()), owner))
    }

    async fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreResult<()> {
        if params.items.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        let persisted_items = into_persisted_rollout_items(params.items, history_mode);
        if persisted_items.is_empty() {
            return Ok(());
        }
        state.calls.append_items += 1;
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .extend(persisted_items);
        Ok(())
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        let mut state = self.state.lock().await;
        state.calls.load_history += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        reject_paginated_history_mode(history_mode)?;
        Ok(StoredThreadHistory {
            revision: None,
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredModelContext> {
        let mut state = self.state.lock().await;
        state.calls.load_latest_model_context += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        Ok(StoredModelContext {
            revision: None,
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread += 1;
        if params.include_history {
            state.calls.read_thread_with_history += 1;
            reject_paginated_history_mode(history_mode_from_state(&state, params.thread_id))?;
        }
        let thread = stored_thread_from_state(&state, params.thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread_by_rollout_path += 1;
        let Some(thread_id) = state.rollout_paths.get(&params.rollout_path).copied() else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "in-memory thread store does not know rollout path {}",
                    params.rollout_path.display()
                ),
            });
        };
        if params.include_history {
            reject_paginated_history_mode(history_mode_from_state(&state, thread_id))?;
        }
        let thread = stored_thread_from_state(&state, thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn list_threads(&self) -> ThreadStoreResult<ThreadPage> {
        let mut state = self.state.lock().await;
        state.calls.list_threads += 1;
        let mut items = state
            .created_threads
            .keys()
            .map(|thread_id| {
                stored_thread_from_state(&state, *thread_id, /*include_history*/ false)
            })
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        items.sort_by_key(|item| item.thread_id.to_string());
        Ok(ThreadPage {
            items,
            next_cursor: state.repeated_list_threads_cursor.clone(),
        })
    }

    async fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreResult<StoredThread> {
        if params.patch.project_id.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "projects",
            });
        }
        let mut state = self.state.lock().await;
        state.calls.update_thread_metadata += 1;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        if let Some(name) = params.patch.name.clone() {
            state.names.insert(params.thread_id, name);
        }
        state
            .metadata_updates
            .entry(params.thread_id)
            .or_default()
            .merge(params.patch);
        stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
    }

    async fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreResult<()> {
        if params
            .section
            .as_deref()
            .is_some_and(|section| section.trim().is_empty())
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "section must not be empty".to_owned(),
            });
        }
        if params.section.is_none() && params.before_thread_id.is_some() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "before thread cannot be specified without a section".to_owned(),
            });
        }

        let mut state = self.state.lock().await;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        let previous_section = state.sections.get(&params.thread_id).cloned();
        let Some(section) = params.section.as_deref() else {
            state.sections.remove(&params.thread_id);
            state.section_positions.remove(&params.thread_id);
            state.section_entered_at.remove(&params.thread_id);
            return Ok(());
        };
        if let Some(before_thread_id) = params.before_thread_id {
            if before_thread_id == params.thread_id {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {} cannot be moved before itself", params.thread_id),
                });
            }
            let before_section = state.sections.get(&before_thread_id).map(String::as_str);
            if before_section != Some(section) {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!(
                        "before thread {before_thread_id} is not in section {section}"
                    ),
                });
            }
        }

        let mut ordered_thread_ids = state
            .sections
            .iter()
            .filter(|(thread_id, current_section)| {
                **thread_id != params.thread_id && current_section.as_str() == section
            })
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        ordered_thread_ids.sort_by_key(|thread_id| {
            (
                state
                    .section_positions
                    .get(thread_id)
                    .copied()
                    .unwrap_or(i64::MAX),
                thread_id.to_string(),
            )
        });
        let insert_at = params
            .before_thread_id
            .and_then(|before_thread_id| {
                ordered_thread_ids
                    .iter()
                    .position(|thread_id| *thread_id == before_thread_id)
            })
            .unwrap_or(ordered_thread_ids.len());
        ordered_thread_ids.insert(insert_at, params.thread_id);
        for (index, thread_id) in ordered_thread_ids.into_iter().enumerate() {
            let position = i64::try_from(index)
                .unwrap_or(i64::MAX)
                .saturating_add(1)
                .saturating_mul(1_000_000);
            state.section_positions.insert(thread_id, position);
        }
        if previous_section.as_deref() != Some(section) {
            state
                .section_entered_at
                .insert(params.thread_id, Utc::now());
            state.sections.insert(params.thread_id, section.to_owned());
        }
        Ok(())
    }

    async fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreResult<()> {
        let (operation, mut state) = self.lock_writer_lifecycle(params.thread_id).await?;
        let store = self.with_state_db(self.state_db.clone());
        crate::store::complete_writer_operations(operation.into_iter().collect(), async move {
            if let Some(owner) = state.writer_owners.get(&params.thread_id) {
                owner.closing();
            }
            state.calls.delete_thread += 1;
            let deleted_state_rows = if let Some(state_db) = &store.state_db {
                state_db
                    .delete_threads_strict(&[params.thread_id])
                    .await
                    .map_err(|error| ThreadStoreError::Internal {
                        message: format!("failed to delete thread state: {error}"),
                    })?
            } else {
                0
            };
            if let Some(owner) = state.writer_owners.get(&params.thread_id) {
                owner.retired();
            }
            state.writer_owners.remove(&params.thread_id);
            state.writer_authority_generations.remove(&params.thread_id);
            state.fresh_writers.remove(&params.thread_id);
            let existed = state.histories.remove(&params.thread_id).is_some();
            state.created_threads.remove(&params.thread_id);
            state.native_authority.remove(&params.thread_id);
            state.names.remove(&params.thread_id);
            state.metadata_updates.remove(&params.thread_id);
            state.sections.remove(&params.thread_id);
            state.section_positions.remove(&params.thread_id);
            state.section_entered_at.remove(&params.thread_id);
            state
                .rollout_paths
                .retain(|_, thread_id| *thread_id != params.thread_id);
            if existed || deleted_state_rows > 0 {
                Ok(())
            } else {
                Err(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })
            }
        })
        .await
    }
}

impl ThreadStore for InMemoryThreadStore {
    fn begin_native_authority_epoch_owned(
        &self,
        epoch: crate::NativeAuthorityEpoch,
        owner: crate::ThreadWriterOwner,
    ) -> ThreadStoreFuture<'_, ()> {
        let store = self.with_state_db(self.state_db.clone());
        Box::pin(async move {
            let operation = owner.operation().await?;
            crate::store::complete_writer_operation(operation, async move {
                epoch
                    .validate()
                    .map_err(|_| ThreadStoreError::InvalidRequest {
                        message: "invalid native authority epoch".into(),
                    })?;
                let mut state = store.state.lock().await;
                if !state
                    .writer_owners
                    .get(&epoch.thread_id)
                    .is_some_and(|current| current.matches(&owner))
                {
                    return Err(ThreadStoreError::Conflict {
                        message: "native authority writer was replaced".into(),
                    });
                }
                if let Some((old, _)) = state.native_authority.get(&epoch.thread_id) {
                    if old.root_thread_id != epoch.root_thread_id
                        || old.parent_thread_id != epoch.parent_thread_id
                    {
                        return Err(ThreadStoreError::Conflict {
                            message: "native authority host ownership changed".into(),
                        });
                    }
                }
                state
                    .writer_authority_generations
                    .insert(epoch.thread_id, epoch.generation);
                state
                    .native_authority
                    .insert(epoch.thread_id, (epoch, None));
                Ok(())
            })
            .await
        })
    }
    fn create_thread_owned(
        &self,
        params: CreateThreadParams,
    ) -> ThreadStoreFuture<'_, crate::ThreadWriterOwner> {
        Box::pin(InMemoryThreadStore::create_thread_owned_impl(
            self, params, /*exclusive*/ true,
        ))
    }
    fn resume_thread_owned(
        &self,
        params: ResumeThreadParams,
        preparation: Option<crate::ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, crate::ThreadWriterOwner)> {
        Box::pin(async move {
            if preparation.is_some() {
                return Err(ThreadStoreError::Unsupported {
                    operation: "resume_thread_with_preparation",
                });
            }
            InMemoryThreadStore::resume_thread_owned_impl(self, params, /*exclusive*/ true).await
        })
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: crate::ThreadWriterOwner,
        discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        let store = self.with_state_db(self.state_db.clone());
        Box::pin(async move {
            let operation = owner.retirement(discard).await?;
            crate::store::complete_writer_operation(operation, async move {
                let mut state = store.state.lock().await;
                if !state
                    .writer_owners
                    .get(&thread)
                    .is_some_and(|current| current.matches(&owner))
                {
                    return Ok(());
                }
                owner.closing();
                if discard
                    && owner.is_provisional()
                    && state.fresh_writers.contains(&thread)
                    && state
                        .native_authority
                        .get(&thread)
                        .is_some_and(|(epoch, _)| {
                            state.writer_authority_generations.get(&thread)
                                != Some(&epoch.generation)
                        })
                {
                    return Err(ThreadStoreError::Conflict {
                        message: "provisional native authority ownership changed".into(),
                    });
                }
                let fresh = state.fresh_writers.contains(&thread);
                if discard && fresh && owner.is_provisional() {
                    state.calls.discard_thread += 1;
                    state.created_threads.remove(&thread);
                    state.histories.remove(&thread);
                    state.metadata_updates.remove(&thread);
                    state.native_authority.remove(&thread);
                } else {
                    state.calls.shutdown_thread += 1;
                }
                owner.retired();
                state.writer_owners.remove(&thread);
                state.writer_authority_generations.remove(&thread);
                state.fresh_writers.remove(&thread);
                Ok(())
            })
            .await
        })
    }
    fn native_authority_storage(&self) -> Option<crate::NativeAuthorityStorage> {
        Some(crate::NativeAuthorityStorage::InMemory {
            allocation: Arc::as_ptr(&self.state) as usize,
        })
    }
    fn begin_native_authority_epoch(
        &self,
        epoch: crate::NativeAuthorityEpoch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            epoch
                .validate()
                .map_err(|_| ThreadStoreError::InvalidRequest {
                    message: "invalid native authority epoch".into(),
                })?;
            let mut state = self.state.lock().await;
            if let Some((old, _)) = state.native_authority.get(&epoch.thread_id)
                && (old.root_thread_id != epoch.root_thread_id
                    || old.parent_thread_id != epoch.parent_thread_id)
            {
                return Err(ThreadStoreError::Conflict {
                    message: "native authority host ownership changed".into(),
                });
            }
            state
                .native_authority
                .insert(epoch.thread_id, (epoch, None));
            Ok(())
        })
    }
    fn invalidate_native_authority_receipt(
        &self,
        thread: ThreadId,
        fence: uuid::Uuid,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if let Some((epoch, receipt)) =
                self.state.lock().await.native_authority.get_mut(&thread)
            {
                epoch.generation = fence;
                *receipt = None;
            }
            Ok(())
        })
    }
    fn seal_native_authority_receipt(
        &self,
        receipt: crate::NativeAuthorityReceipt,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async move {
            receipt
                .validate()
                .map_err(|_| ThreadStoreError::InvalidRequest {
                    message: "invalid native authority receipt".into(),
                })?;
            let mut state = self.state.lock().await;
            let Some((epoch, sealed)) = state.native_authority.get_mut(&receipt.epoch.thread_id)
            else {
                return Ok(false);
            };
            if epoch != &receipt.epoch {
                return Ok(false);
            }
            *sealed = Some(receipt);
            Ok(true)
        })
    }
    fn read_native_authority_epoch(
        &self,
        thread: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::NativeAuthorityEpoch>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .await
                .native_authority
                .get(&thread)
                .map(|(epoch, _)| epoch.clone()))
        })
    }
    fn read_native_authority_receipt(
        &self,
        thread: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::NativeAuthorityReceipt>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .await
                .native_authority
                .get(&thread)
                .and_then(|(_, sealed)| sealed.clone()))
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            InMemoryThreadStore::create_thread_owned_impl(self, params, /*exclusive*/ false)
                .await
                .map(|_| ())
        })
    }

    fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>> {
        Box::pin(async move {
            InMemoryThreadStore::resume_thread_owned_impl(self, params, /*exclusive*/ false)
                .await
                .map(|(history, _)| history)
        })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::append_items(self, params))
    }

    fn persist_thread(
        &self,
        _thread_id: ThreadId,
        _context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.persist_thread += 1;
            Ok(())
        })
    }

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.flush_thread += 1;
            Ok(())
        })
    }

    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let (operation, mut state) = self.lock_writer_lifecycle(thread_id).await?;
            let mut operation = operation;
            if let Some(operation) = operation.as_mut() {
                operation.legacy_intent(/*discard*/ false)?;
                operation.start_retirement();
            }
            state.calls.shutdown_thread += 1;
            if let Some(operation) = operation.as_ref() {
                operation.retired();
            }
            state.writer_owners.remove(&thread_id);
            state.fresh_writers.remove(&thread_id);
            state.writer_authority_generations.remove(&thread_id);
            if let Some(operation) = operation {
                operation.complete();
            }
            Ok(())
        })
    }

    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let (operation, mut state) = self.lock_writer_lifecycle(thread_id).await?;
            let mut operation = operation;
            if let Some(operation) = operation.as_mut() {
                operation.legacy_intent(/*discard*/ true)?;
                operation.start_retirement();
            }
            state.calls.discard_thread += 1;
            if let Some(operation) = operation.as_ref() {
                operation.retired();
            }
            state.writer_owners.remove(&thread_id);
            state.fresh_writers.remove(&thread_id);
            state.writer_authority_generations.remove(&thread_id);
            if let Some(operation) = operation {
                operation.complete();
            }
            Ok(())
        })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(InMemoryThreadStore::load_history(self, params))
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(InMemoryThreadStore::load_latest_model_context(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread(self, params))
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread_by_rollout_path(
            self, params,
        ))
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async move {
            let mut page = InMemoryThreadStore::list_threads(self).await?;
            match params.relation_filter {
                Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
                    page.items
                        .retain(|thread| thread.parent_thread_id == Some(parent_thread_id));
                }
                Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
                    let mut subtree = HashSet::from([ancestor_thread_id]);
                    loop {
                        let mut discovered = false;
                        for thread in &page.items {
                            if thread
                                .parent_thread_id
                                .is_some_and(|parent_thread_id| subtree.contains(&parent_thread_id))
                            {
                                discovered |= subtree.insert(thread.thread_id);
                            }
                        }
                        if !discovered {
                            break;
                        }
                    }
                    page.items.retain(|thread| {
                        thread.thread_id != ancestor_thread_id
                            && subtree.contains(&thread.thread_id)
                    });
                }
                None => {}
            }
            if let Some(section) = params.section.as_ref() {
                page.items.retain(|thread| {
                    thread.section.as_ref().map(|section| section.id.as_str()) == section.as_deref()
                });
            }
            if params.sort_key == ThreadSortKey::SectionPosition {
                page.items.sort_by_key(|thread| {
                    (
                        thread.section_position.unwrap_or(i64::MAX),
                        thread.thread_id.to_string(),
                    )
                });
                if params.sort_direction == crate::SortDirection::Desc {
                    page.items.reverse();
                }
            }
            Ok(page)
        })
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            let updated = InMemoryThreadStore::update_thread_metadata(self, params).await?;
            Ok((!self.omit_metadata_update_result.load(Ordering::Relaxed)).then_some(updated))
        })
    }

    fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::move_thread_to_section(self, params))
    }

    fn archive_thread(&self, _params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.archive_thread += 1;
            Ok(())
        })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.unarchive_thread += 1;
            stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
        })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::delete_thread(self, params))
    }
}

fn stored_thread_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
    include_history: bool,
) -> ThreadStoreResult<StoredThread> {
    let created = state
        .created_threads
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let history_items = state.histories.get(&thread_id).cloned().unwrap_or_default();
    let history = include_history.then(|| StoredThreadHistory {
        revision: None,
        thread_id,
        items: history_items.clone(),
    });
    let name = state.names.get(&thread_id).cloned().flatten();
    let metadata = state.metadata_updates.get(&thread_id);
    let rollout_path = state
        .rollout_paths
        .iter()
        .find_map(|(path, mapped_thread_id)| {
            (*mapped_thread_id == thread_id).then(|| path.clone())
        });

    Ok(StoredThread {
        originator: (!created.originator.is_empty()).then(|| created.originator.clone()),
        thread_id,
        extra_config: created.extra_config.clone(),
        rollout_path: metadata
            .and_then(|metadata| metadata.rollout_path.clone())
            .or(rollout_path),
        forked_from_id: created.forked_from_id,
        parent_thread_id: created.parent_thread_id,
        preview: metadata
            .and_then(|metadata| metadata.preview.clone())
            .unwrap_or_default(),
        name,
        model_provider: metadata
            .and_then(|metadata| metadata.model_provider.clone())
            .unwrap_or_else(|| "test".to_string()),
        model: metadata.and_then(|metadata| metadata.model.clone()),
        reasoning_effort: metadata
            .and_then(|metadata| metadata.reasoning_effort.clone())
            .flatten(),
        created_at: metadata
            .and_then(|metadata| metadata.created_at)
            .unwrap_or_else(Utc::now),
        updated_at: metadata
            .and_then(|metadata| metadata.updated_at)
            .unwrap_or_else(Utc::now),
        recency_at: metadata
            .and_then(|metadata| metadata.advance_recency_at.or(metadata.updated_at))
            .unwrap_or_else(Utc::now),
        archived_at: None,
        section: state
            .sections
            .get(&thread_id)
            .cloned()
            .map(|id| codex_state::ThreadSection {
                name: if id == codex_state::PINNED_THREAD_SECTION_ID {
                    codex_state::PINNED_THREAD_SECTION_NAME.to_string()
                } else {
                    id.clone()
                },
                id,
                appearance: None,
            }),
        section_position: state.section_positions.get(&thread_id).copied(),
        section_entered_at: state.section_entered_at.get(&thread_id).copied(),
        project_id: None,
        daybreak_enabled: metadata.and_then(|metadata| metadata.daybreak_enabled),
        cwd: metadata
            .and_then(|metadata| metadata.cwd.clone())
            .unwrap_or_default(),
        cli_version: metadata
            .and_then(|metadata| metadata.cli_version.clone())
            .unwrap_or_else(|| "test".to_string()),
        source: metadata
            .and_then(|metadata| metadata.source.clone())
            .unwrap_or_else(|| created.source.clone()),
        history_mode: created.history_mode,
        thread_source: metadata
            .and_then(|metadata| metadata.thread_source.clone())
            .unwrap_or_else(|| created.thread_source.clone()),
        agent_nickname: metadata.and_then(|metadata| metadata.agent_nickname.clone().flatten()),
        agent_role: metadata.and_then(|metadata| metadata.agent_role.clone().flatten()),
        agent_path: metadata.and_then(|metadata| metadata.agent_path.clone().flatten()),
        git_info: metadata.and_then(git_info_from_patch),
        approval_mode: metadata
            .and_then(|metadata| metadata.approval_mode)
            .unwrap_or(AskForApproval::Never),
        permission_profile: metadata
            .and_then(|metadata| metadata.permission_profile.clone())
            .unwrap_or_else(PermissionProfile::read_only),
        token_usage: metadata.and_then(|metadata| metadata.token_usage.clone()),
        first_user_message: metadata.and_then(|metadata| metadata.first_user_message.clone()),
        history,
    })
}

fn history_mode_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
) -> ThreadHistoryMode {
    state
        .created_threads
        .get(&thread_id)
        .map(|thread| thread.history_mode)
        .unwrap_or_default()
}

fn git_info_from_patch(patch: &ThreadMetadataPatch) -> Option<codex_protocol::protocol::GitInfo> {
    let git_info = patch.git_info.as_ref()?;
    let sha = git_info.sha.clone().flatten();
    let branch = git_info.branch.clone().flatten();
    let origin_url = git_info.origin_url.clone().flatten();
    if sha.is_none() && branch.is_none() && origin_url.is_none() {
        return None;
    }
    Some(codex_protocol::protocol::GitInfo {
        commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
        branch,
        repository_url: origin_url,
    })
}
