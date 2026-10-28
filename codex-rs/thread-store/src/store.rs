use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::AddThreadAttachmentOutcome;
use crate::AddThreadAttachmentParams;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ArchiveThreadsParams;
use crate::CreateProjectParams;
use crate::CreateThreadParams;
use crate::CreateThreadSectionParams;
use crate::CreatedProject;
use crate::DeleteThreadParams;
use crate::DeleteThreadSectionParams;
use crate::DeleteThreadsParams;
use crate::DeletedProject;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListProjectsParams;
use crate::ListThreadAttachmentThreadsParams;
use crate::ListThreadAttachmentsParams;
use crate::ListThreadSectionsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::MoveProjectParams;
use crate::MoveThreadToSectionParams;
use crate::PrepareForkParams;
use crate::PreparedFork;
use crate::ProjectMoveOutcome;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::RemoveThreadAttachmentOutcome;
use crate::RemoveThreadAttachmentParams;
use crate::RenameThreadSectionParams;
use crate::ResumeThreadParams;
use crate::RevertThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadAttachmentOwnerPage;
use crate::ThreadAttachmentPage;
use crate::ThreadMetadataPatch;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;
use crate::UpdateProjectParams;
use crate::UpdateThreadMetadataParams;
use crate::UpdatedProject;

/// Future returned by [`ThreadStore`] operations.
pub type ThreadStoreFuture<'a, T> = Pin<Box<dyn Future<Output = ThreadStoreResult<T>> + Send + 'a>>;

/// Data a [`ThreadStore`] has already loaded while preparing a thread for resume.
///
/// Core does not inspect this value. It gives the value back to the store when reopening the
/// thread, allowing the store to reuse the loaded history and writer state instead of loading them
/// again. The value can only be used once because it may own resources that cannot be copied.
pub struct ThreadStoreResumePreparation {
    state: Box<dyn Any + Send>,
}

impl ThreadStoreResumePreparation {
    pub fn new<T: Any + Send>(state: T) -> Self {
        Self {
            state: Box::new(state),
        }
    }

    pub fn downcast<T: Any + Send>(self) -> Result<T, Self> {
        match self.state.downcast::<T>() {
            Ok(state) => Ok(*state),
            Err(state) => Err(Self { state }),
        }
    }
}

impl fmt::Debug for ThreadStoreResumePreparation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadStoreResumePreparation")
            .finish_non_exhaustive()
    }
}

/// Why thread persistence is being requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistContext {
    /// Thread initialization or context injection is complete without starting a turn.
    ThreadPreparation,
    /// Standard persistence makes the thread and all queued items durable and readable.
    Standard,
    /// Copied subagent history may be enqueued so its durability fence can overlap relationship
    /// persistence. The caller must await standard persistence before acknowledging the child.
    SubagentSpawn,
    /// A turn is about to begin sampling after its input has been recorded.
    TurnStart,
    /// Accepted user input is being recorded before an active turn's next sampling request.
    /// This does not apply to tool outputs, cancellation, or task cleanup.
    SteeredUserInput,
}

impl PersistContext {
    /// Whether a store may enqueue this checkpoint before returning and fence it at a later
    /// durability barrier. Stores may still choose to persist synchronously.
    pub fn allows_background_persistence(self) -> bool {
        match self {
            Self::ThreadPreparation | Self::Standard => false,
            Self::SubagentSpawn | Self::TurnStart | Self::SteeredUserInput => true,
        }
    }
}

/// Actual host-store provenance, never derived from thread/model metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeAuthorityStorage {
    Local {
        directory: std::path::PathBuf,
        database: std::path::PathBuf,
    },
    InMemory {
        allocation: usize,
    },
}

/// Storage-neutral thread persistence boundary.
/// Store-generated identity only; it grants no native provider authority and
/// does not retain the operating system writer lock. Built-in stores share its
/// private operation lease with every writer retirement/replacement path; an
/// admitted LiveThread mutation owns that lease through storage and metadata
/// acknowledgment. Legacy handles without an owner keep their existing contract.
/// An incomplete task fails closed rather than treating a dropped future as an
/// I/O drain. This adds no native authority or serialized identity.
#[derive(Clone, Debug)]
pub struct ThreadWriterOwner(Arc<WriterLifetime>, Arc<std::sync::atomic::AtomicBool>);

#[derive(Debug)]
struct WriterLifetime {
    operation: Arc<tokio::sync::Mutex<()>>,
    phase: std::sync::atomic::AtomicU8,
    teardown_intent: std::sync::atomic::AtomicU8,
    #[cfg(test)]
    wait_observer: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    #[cfg(test)]
    bypass_lifecycle: std::sync::atomic::AtomicBool,
}

const WRITER_ACTIVE: u8 = 0;
const WRITER_CLOSING: u8 = 1;
const WRITER_RETIRED: u8 = 2;
const WRITER_INCOMPLETE: u8 = 3;

/// A task dropping this lease without completing may have queued storage work.
/// Keep its writer fail-closed: unlocking a Rust mutex is not an I/O drain.
#[must_use = "an admitted writer operation must be run or dropped before effects"]
pub struct ThreadWriterOperation {
    owner: ThreadWriterOwner,
    _guard: tokio::sync::OwnedMutexGuard<()>,
    completed: bool,
    armed: bool,
    intent: Option<u8>,
    live: Arc<std::sync::atomic::AtomicBool>,
}

/// An admitted exact-writer lease. Acquire it before store locks. Dropping it
/// before run has no effects; run owns actual I/O and acknowledgment even if its
/// caller is cancelled. Normal errors retain Closing for identical cleanup retry;
/// panic/incomplete work fails closed. Never spawn I/O that outlives run's future.
impl ThreadWriterOperation {
    /// The only public completion path. Store state/writer locks belong INSIDE
    /// this future, after owner admission; never await admission under those locks.
    pub async fn run<T: Send + 'static>(
        self,
        future: impl Future<Output = ThreadStoreResult<T>> + Send + 'static,
    ) -> ThreadStoreResult<T> {
        complete_writer_operation(self, future).await
    }
    /// Obtain a retirement marker before moving the lease into run. Call retire
    /// only after all cleanup effects finish, under the store's own exact-entry
    /// lock, immediately before removing that entry. Mutation leases have no marker.
    pub fn retirement_marker(&self) -> ThreadStoreResult<ThreadWriterRetirement> {
        if self.intent.is_none() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "mutation lease cannot retire a writer".into(),
            });
        }
        Ok(ThreadWriterRetirement {
            owner: self.owner.clone(),
            live: Arc::downgrade(&self.live),
        })
    }
    pub(crate) fn require_active(&self) -> ThreadStoreResult<()> {
        if self
            .owner
            .0
            .phase
            .load(std::sync::atomic::Ordering::Acquire)
            != WRITER_ACTIVE
        {
            return Err(ThreadStoreError::Conflict {
                message: "writer teardown must complete before replacement".into(),
            });
        }
        Ok(())
    }
    pub(crate) fn legacy_intent(&mut self, discard: bool) -> ThreadStoreResult<()> {
        let wanted = if discard { 4 } else { 3 };
        let phase = self
            .owner
            .0
            .phase
            .load(std::sync::atomic::Ordering::Acquire);
        if phase == WRITER_CLOSING
            && self
                .owner
                .0
                .teardown_intent
                .load(std::sync::atomic::Ordering::Acquire)
                != wanted
        {
            return Err(ThreadStoreError::Conflict {
                message: "legacy release cannot bypass another teardown intent".into(),
            });
        }
        self.intent = Some(wanted);
        Ok(())
    }
    pub(crate) fn start_retirement(&mut self) {
        self.owner.0.teardown_intent.store(
            self.intent.unwrap_or(5),
            std::sync::atomic::Ordering::Release,
        );
        self.armed = true;
        self.closing();
    }
    pub(crate) fn complete(mut self) {
        self.completed = true;
    }
    pub(crate) fn closing(&self) {
        self.owner
            .0
            .phase
            .store(WRITER_CLOSING, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn retired(&self) {
        self.owner
            .0
            .phase
            .store(WRITER_RETIRED, std::sync::atomic::Ordering::Release);
    }
}

/// Opaque exact-writer retirement marker, valid only inside its live run task.
/// It cannot release the operation lease or certify completion of unfinished I/O.
pub struct ThreadWriterRetirement {
    owner: ThreadWriterOwner,
    live: std::sync::Weak<std::sync::atomic::AtomicBool>,
}
impl ThreadWriterRetirement {
    pub fn retire(&self) -> ThreadStoreResult<()> {
        if !self
            .live
            .upgrade()
            .is_some_and(|live| live.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(ThreadStoreError::Conflict {
                message: "retirement marker has no running operation".into(),
            });
        }
        self.owner.retired();
        Ok(())
    }
}

pub(crate) type WriterOperation = ThreadWriterOperation;

impl Drop for ThreadWriterOperation {
    fn drop(&mut self) {
        self.live.store(false, std::sync::atomic::Ordering::Release);
        if self.armed && !self.completed {
            self.owner
                .0
                .phase
                .store(WRITER_INCOMPLETE, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Admission happens in the caller before spawning. Cancellation while waiting
/// cannot enqueue a later unwanted operation; after admission the task owns all
/// real store awaits and the caller may stop waiting without abandoning I/O.
pub(crate) async fn complete_writer_operation<T: Send + 'static>(
    operation: WriterOperation,
    future: impl Future<Output = ThreadStoreResult<T>> + Send + 'static,
) -> ThreadStoreResult<T> {
    complete_writer_operations(vec![operation], future).await
}

pub(crate) async fn complete_writer_operations<T: Send + 'static>(
    mut operations: Vec<WriterOperation>,
    future: impl Future<Output = ThreadStoreResult<T>> + Send + 'static,
) -> ThreadStoreResult<T> {
    tokio::spawn(async move {
        for operation in &mut operations {
            operation.armed = true;
            operation
                .live
                .store(true, std::sync::atomic::Ordering::Release);
            if let Some(intent) = operation.intent {
                operation
                    .owner
                    .0
                    .teardown_intent
                    .store(intent, std::sync::atomic::Ordering::Release);
                if operation
                    .owner
                    .0
                    .phase
                    .load(std::sync::atomic::Ordering::Acquire)
                    != WRITER_RETIRED
                {
                    operation.closing();
                }
            }
        }
        let result = future.await;
        for operation in &mut operations {
            operation.completed = true;
        }
        result
    })
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("owned writer operation did not complete: {error}"),
    })?
}
impl ThreadWriterOwner {
    pub fn new() -> Self {
        Self(
            Arc::new(WriterLifetime {
                operation: Arc::new(tokio::sync::Mutex::new(())),
                phase: std::sync::atomic::AtomicU8::new(WRITER_ACTIVE),
                teardown_intent: std::sync::atomic::AtomicU8::new(0),
                #[cfg(test)]
                wait_observer: std::sync::Mutex::new(None),
                #[cfg(test)]
                bypass_lifecycle: std::sync::atomic::AtomicBool::new(false),
            }),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )
    }
    // Call only while holding this identity's operation lease.
    pub(crate) fn closing(&self) {
        self.0
            .phase
            .store(WRITER_CLOSING, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn retired(&self) {
        self.0
            .phase
            .store(WRITER_RETIRED, std::sync::atomic::Ordering::Release);
    }
    /// Admit an Active mutation. Await before taking the store's own locks;
    /// pass all actual effects and acknowledgment to the returned lease's run.
    pub async fn operation(&self) -> ThreadStoreResult<ThreadWriterOperation> {
        self.acquire_operation(/*retirement*/ false).await
    }
    pub(crate) async fn lifecycle(&self) -> ThreadStoreResult<WriterOperation> {
        self.acquire_operation(/*retirement*/ true).await
    }
    /// Admit exact cleanup. A Closing retry must keep the original discard/shutdown
    /// intent. Independent stores must use this protocol for owned finish and
    /// protect every legacy replacement/release path with the same identity lease.
    pub async fn retirement(&self, discard: bool) -> ThreadStoreResult<ThreadWriterOperation> {
        let mut operation = self.acquire_operation(/*retirement*/ true).await?;
        let phase = self.0.phase.load(std::sync::atomic::Ordering::Acquire);
        let intent = self
            .0
            .teardown_intent
            .load(std::sync::atomic::Ordering::Acquire);
        if phase == WRITER_CLOSING && intent != if discard { 2 } else { 1 } {
            return Err(ThreadStoreError::Conflict {
                message: "writer cleanup retry changed teardown intent".into(),
            });
        }
        operation.intent = Some(if discard { 2 } else { 1 });
        Ok(operation)
    }
    /// Legacy ID-selected release/replacement must also drain the selected exact
    /// owner. It may retry only its own identical legacy teardown, never a failed
    /// owned cleanup. Recheck the selected entry after admission before effects.
    pub async fn legacy_retirement(
        &self,
        discard: bool,
    ) -> ThreadStoreResult<ThreadWriterOperation> {
        let mut operation = self.acquire_operation(/*retirement*/ true).await?;
        operation.legacy_intent(discard)?;
        Ok(operation)
    }
    #[cfg(test)]
    pub(crate) fn watch_lease_wait(&self) -> Arc<tokio::sync::Notify> {
        let notify = Arc::new(tokio::sync::Notify::new());
        *self.0.wait_observer.lock().expect("writer observer") = Some(notify.clone());
        notify
    }
    #[cfg(test)]
    pub(crate) fn bypass_lifecycle_for_test(&self) {
        self.0
            .bypass_lifecycle
            .store(true, std::sync::atomic::Ordering::Release);
    }
    async fn acquire_operation(&self, retirement: bool) -> ThreadStoreResult<WriterOperation> {
        #[cfg(not(test))]
        let guard = self.0.operation.clone().lock_owned().await;
        #[cfg(test)]
        let guard = {
            // Deliberately broken lease-negative variant retains all real store
            // effects. It exists only to prove the race harness detects bypass.
            let mutex = if retirement
                && self
                    .0
                    .bypass_lifecycle
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                Arc::new(tokio::sync::Mutex::new(()))
            } else {
                self.0.operation.clone()
            };
            let lock = mutex.lock_owned();
            tokio::pin!(lock);
            std::future::poll_fn(|cx| {
                let polled = lock.as_mut().poll(cx);
                if polled.is_pending() {
                    if let Some(observer) = self
                        .0
                        .wait_observer
                        .lock()
                        .expect("writer observer")
                        .as_ref()
                    {
                        observer.notify_one();
                    }
                }
                polled
            })
            .await
        };
        let phase = self.0.phase.load(std::sync::atomic::Ordering::Acquire);
        if phase == WRITER_INCOMPLETE || (!retirement && phase != WRITER_ACTIVE) {
            return Err(ThreadStoreError::Conflict {
                message: "thread writer is retired, closing, or has incomplete storage work".into(),
            });
        }
        Ok(ThreadWriterOperation {
            owner: self.clone(),
            _guard: guard,
            completed: false,
            armed: false,
            intent: None,
            live: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    pub fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn provisional(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.1)
    }
    pub fn is_provisional(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Default for ThreadWriterOwner {
    fn default() -> Self {
        Self::new()
    }
}

pub trait ThreadStore: Any + Send + Sync {
    fn begin_native_authority_epoch_owned(
        &self,
        _epoch: crate::NativeAuthorityEpoch,
        _owner: ThreadWriterOwner,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }
    /// Ownership of one exact writer acquisition, returned atomically by the
    /// store. Unknown stores fail before acquiring a writer.
    fn create_thread_owned(
        &self,
        _params: CreateThreadParams,
    ) -> ThreadStoreFuture<'_, ThreadWriterOwner> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "owned_writer",
            })
        })
    }
    fn resume_thread_owned(
        &self,
        _params: ResumeThreadParams,
        _preparation: Option<ThreadStoreResumePreparation>,
    ) -> ThreadStoreFuture<'_, (Arc<Vec<RolloutItem>>, ThreadWriterOwner)> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "owned_writer",
            })
        })
    }
    fn finish_thread_owned(
        &self,
        _thread: ThreadId,
        _owner: ThreadWriterOwner,
        _discard: bool,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "owned_writer",
            })
        })
    }
    /// Unknown stores cannot provide native cold authority proof.
    fn native_authority_storage(&self) -> Option<NativeAuthorityStorage> {
        None
    }
    fn begin_native_authority_epoch(
        &self,
        _epoch: crate::NativeAuthorityEpoch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }
    fn invalidate_native_authority_receipt(
        &self,
        _thread: ThreadId,
        _fence: uuid::Uuid,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }
    fn seal_native_authority_receipt(
        &self,
        _receipt: crate::NativeAuthorityReceipt,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }
    fn read_native_authority_epoch(
        &self,
        _thread: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::NativeAuthorityEpoch>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }
    fn read_native_authority_receipt(
        &self,
        _thread: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<crate::NativeAuthorityReceipt>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "native_authority",
            })
        })
    }

    /// Return this store as [`Any`] for implementation-owned escape hatches.
    fn as_any(&self) -> &dyn Any;

    /// Returns the history mode to use when history does not carry a persisted mode.
    ///
    /// The default is legacy so existing stores stay compatible. Stores whose durable contract is
    /// already paginated should override this instead of relying on core to infer storage behavior.
    fn default_history_mode(&self) -> ThreadHistoryMode {
        ThreadHistoryMode::Legacy
    }

    /// Creates a new live thread.
    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Stages host-owned metadata for a thread ID reserved before Core starts the thread.
    ///
    /// The entry remains in memory until the first successful metadata update for that thread.
    /// Callers must remove it if startup fails before the store opens a live thread.
    fn stage_pending_thread_metadata(
        &self,
        _thread_id: ThreadId,
        _patch: ThreadMetadataPatch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "stage_pending_thread_metadata",
            })
        })
    }

    /// Reads metadata staged for a reserved thread without persisting it.
    fn read_pending_thread_metadata(
        &self,
        _thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<ThreadMetadataPatch>> {
        Box::pin(async { Ok(None) })
    }

    /// Removes host-owned metadata staged for a reserved thread ID.
    fn remove_pending_thread_metadata(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "remove_pending_thread_metadata",
            })
        })
    }

    /// Reopens an existing thread for live appends and returns its authoritative replay context.
    ///
    /// Stored snapshots supplied by callers may predate writer acquisition. Implementations must
    /// include writes committed before acquisition in the returned context. Explicitly supplied
    /// histories without a canonical session header retain their import/override semantics.
    /// A supplied revision may enable reuse of the shared snapshot after validation; missing or
    /// unrecognized revisions must not be treated as proof that stored history is unchanged.
    /// On failure, implementations must release any writer acquired by this operation.
    fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>>;

    /// Reopens a thread using implementation-owned state prepared before Core startup.
    ///
    /// Stores should only override this when they also produce the preparation. The returned
    /// history and failure cleanup guarantees are the same as [`Self::resume_thread`].
    fn resume_thread_with_preparation(
        &self,
        _params: ResumeThreadParams,
        _preparation: ThreadStoreResumePreparation,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "resume_thread_with_preparation",
            })
        })
    }

    /// Appends raw rollout items to a live thread.
    ///
    /// Implementations should apply the shared rollout persistence policy before writing durable
    /// replay history and before updating any implementation-owned projections.
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()>;

    /// Records metadata derived from rollout items.
    /// Unlike [`Self::update_thread_metadata`], this allows the write to be deferred.
    ///
    /// The default implementation awaits the update. Deferred writes must respect the
    /// flush and shutdown guarantees documented by [`Self::persist_thread`].
    fn record_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { self.update_thread_metadata(params).await.map(|_| ()) })
    }

    /// Materializes the thread if persistence is lazy, then persists all queued items.
    ///
    /// Preparation checkpoints may leave disposable preparation data in memory without
    /// activating storage. Standard persistence must complete before returning. Contexts that allow background
    /// persistence may complete asynchronously when the implementation enqueues the checkpoint
    /// before returning, fences it with subsequent flush or shutdown operations, and surfaces
    /// failures through those operations.
    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()>;

    /// Flushes all queued items and returns once they are durable/readable.
    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Flushes pending items and closes the live thread writer.
    /// Stores with opt-in disposable preparations may discard an unactivated preparation.
    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Discards the live thread writer without forcing pending in-memory items to become durable.
    ///
    /// Core calls this when session initialization fails after a live writer has been created.
    /// Implementations should release any live writer resources for the thread while preserving
    /// already-durable thread data.
    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Loads persisted history for resume, fork, and memory jobs.
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory>;

    /// Loads the persisted rollout items needed to reconstruct the latest model-visible context.
    ///
    /// Implementations that cannot perform a targeted read may return the full persisted history.
    fn load_latest_model_context(
        &self,
        _params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_latest_model_context",
            })
        })
    }

    /// Freezes source history and model context used to initialize a referenced fork.
    ///
    /// Stores without reference-backed fork support can retain this default implementation.
    fn prepare_fork(&self, _params: PrepareForkParams) -> ThreadStoreFuture<'_, PreparedFork> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "prepare_fork",
            })
        })
    }

    /// Reverts a paginated thread's durable history so it ends immediately before
    /// `before_turn_id`.
    ///
    /// Callers must close the thread's live writer first. The logical thread id and semantic
    /// metadata stay unchanged.
    ///
    /// Stores without paginated revert support can retain this default implementation.
    fn revert_thread(&self, _params: RevertThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "revert_thread",
            })
        })
    }

    /// Reads a thread summary and optionally its persisted history.
    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Reads a rollout-backed thread by path when the store supports path-addressed lookups.
    ///
    /// Deprecated: new callers should use [`ThreadStore::read_thread`] instead.
    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread>;

    /// Lists stored threads matching the supplied filters.
    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage>;

    /// Whether this store can discover and manage independently persisted thread sections.
    fn supports_thread_sections(&self) -> bool {
        false
    }

    /// Lists independently persisted thread sections.
    fn list_thread_sections(
        &self,
        _params: ListThreadSectionsParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSectionsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/list",
            })
        })
    }

    /// Creates a custom thread section with a stable, server-assigned identity.
    fn create_thread_section(
        &self,
        _params: CreateThreadSectionParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSection> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/create",
            })
        })
    }

    /// Renames a custom thread section, returning `None` when it does not exist.
    fn rename_thread_section(
        &self,
        _params: RenameThreadSectionParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThreadSection>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/update",
            })
        })
    }

    /// Deletes a custom thread section and reports whether it existed.
    fn delete_thread_section(
        &self,
        _params: DeleteThreadSectionParams,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/delete",
            })
        })
    }

    /// Whether this store can persist and discover thread-owned attachments.
    fn supports_thread_attachments(&self) -> bool {
        false
    }

    /// Copies current attachment membership into a newly persisted fork.
    ///
    /// Copies must be atomic and use new attachment IDs. The destination must be empty;
    /// subsequent membership changes on either thread must remain independent.
    fn copy_thread_attachments(
        &self,
        _source_thread_id: ThreadId,
        _destination_thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "copy_thread_attachments",
            })
        })
    }

    /// Attaches an attachment, returning an existing attachment for repeated requests.
    fn add_thread_attachment(
        &self,
        _params: AddThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, AddThreadAttachmentOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/add",
            })
        })
    }

    /// Lists attachments belonging to one persisted thread.
    fn list_thread_attachments(
        &self,
        _params: ListThreadAttachmentsParams,
    ) -> ThreadStoreFuture<'_, ThreadAttachmentPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/list",
            })
        })
    }

    /// Lists owners of an exact attachment identity, without thread browser visibility filters.
    /// Results describe persisted membership, not an atomic claim on the referenced resource.
    fn list_thread_attachment_threads(
        &self,
        _params: ListThreadAttachmentThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadAttachmentOwnerPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachmentOwner/list",
            })
        })
    }

    /// Removes an attachment and reports whether it existed.
    fn remove_thread_attachment(
        &self,
        _params: RemoveThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, RemoveThreadAttachmentOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/attachment/remove",
            })
        })
    }

    /// Whether this store supports durable host-owned projects.
    fn supports_projects(&self) -> bool {
        false
    }

    fn list_projects(
        &self,
        _params: ListProjectsParams,
    ) -> ThreadStoreFuture<'_, StoredProjectsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/list",
            })
        })
    }

    fn read_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<StoredProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/read",
            })
        })
    }

    fn create_project(
        &self,
        _params: CreateProjectParams,
    ) -> ThreadStoreFuture<'_, CreatedProject> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/create",
            })
        })
    }

    fn update_project(
        &self,
        _params: UpdateProjectParams,
    ) -> ThreadStoreFuture<'_, Option<UpdatedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/update",
            })
        })
    }

    fn move_project(
        &self,
        _params: MoveProjectParams,
    ) -> ThreadStoreFuture<'_, Option<ProjectMoveOutcome>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/move",
            })
        })
    }

    fn delete_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<DeletedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/delete",
            })
        })
    }

    /// Whether paginated threads can hydrate durable history through turn and item lists.
    fn supports_paginated_history_lists(&self) -> bool {
        false
    }

    /// Searches stored threads and returns search-only preview metadata.
    fn search_threads(
        &self,
        _params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/search",
            })
        })
    }

    /// Searches visible message occurrences within one paginated thread.
    fn search_thread_occurrences(
        &self,
        _params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreFuture<'_, ThreadOccurrenceSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/searchOccurrences",
            })
        })
    }

    /// Lists turns within a stored thread.
    fn list_turns(&self, _params: ListTurnsParams) -> ThreadStoreFuture<'_, TurnPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_turns",
            })
        })
    }

    /// Lists persisted items within a stored thread, optionally filtered to a turn.
    fn list_items(&self, _params: ListItemsParams) -> ThreadStoreFuture<'_, ItemPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_items",
            })
        })
    }

    /// Lists bounded ordinary and realtime thread history in rollout order.
    fn list_timeline(
        &self,
        _params: crate::ListTimelineParams,
    ) -> ThreadStoreFuture<'_, crate::TimelinePage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/timeline/list",
            })
        })
    }

    /// Applies a literal metadata patch and returns the updated thread when one was materialized.
    ///
    /// `None` means the update succeeded without materializing a thread, for example because the
    /// implementation filtered the patch to a no-op. Callers that require a `StoredThread` must
    /// perform a fallback read.
    ///
    /// Implementations should apply the supplied fields directly. Policy such as deciding whether
    /// an append-derived preview should be emitted belongs above the store.
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>>;

    /// Moves a thread to, within, or out of a server-ordered section.
    fn move_thread_to_section(
        &self,
        _params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/section/move",
            })
        })
    }

    /// Archives a thread.
    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Archives threads in order, returning the successfully archived thread ids.
    ///
    /// The first thread must archive successfully; later failures are best effort.
    fn archive_threads(
        &self,
        params: ArchiveThreadsParams,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            let mut archived_thread_ids = Vec::new();
            for thread_id in params.thread_ids {
                match self.archive_thread(ArchiveThreadParams { thread_id }).await {
                    Ok(()) => archived_thread_ids.push(thread_id),
                    Err(err) if archived_thread_ids.is_empty() => return Err(err),
                    Err(err) => tracing::warn!("failed to archive thread {thread_id}: {err}"),
                }
            }
            Ok(archived_thread_ids)
        })
    }

    /// Unarchives a thread and returns its updated metadata.
    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Deletes a thread's persisted rollout data and associated metadata.
    /// Success includes cleanup of associated persisted state; callers must not repeat it.
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Deletes threads and their associated persisted state in order, treating already-missing
    /// members as deleted.
    ///
    /// Stores with request-scoped delete preflight should override this instead of repeating
    /// that work through [`ThreadStore::delete_thread`].
    fn delete_threads(&self, params: DeleteThreadsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            for thread_id in params.thread_ids {
                match self.delete_thread(DeleteThreadParams { thread_id }).await {
                    Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        })
    }
}
