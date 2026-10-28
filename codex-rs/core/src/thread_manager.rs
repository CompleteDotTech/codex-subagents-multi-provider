mod managed;
mod shared_instructions;
mod shutdown;

pub use shutdown::AgentTreeShutdown;
pub use shutdown::AgentTreeShutdownFailure;
pub use shutdown::AgentTreeShutdownFailureReason;
pub use shutdown::AgentTreeShutdownReport;
pub(crate) use shutdown::thread_store_error_kind;

use crate::CodexAppsToolsCache;
use crate::CodexThreadSettingsOverrides;
use crate::agent::AgentStatus;
use crate::agent::LocalAgentControl;
use crate::agent::api::AgentConfigUpdate;
use crate::agent::api::AgentControl;
use crate::agent::control::AgentControlInit;
use crate::agent::control::LocalAgentRuntime;
use crate::agents_md_manager::SessionInstructions;
use crate::attestation::AttestationProvider;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::ThreadStoreConfig;
use crate::current_time::TimeProvider;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::default_thread_environment_requests;
use crate::mcp::McpManager;
use crate::rollout::truncation;
use crate::session::ForkPersistence;
use crate::session::GitEnrichmentPolicy;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::SessionIo;
use crate::session::SessionSpawnArgs;
use crate::session::resolve_multi_agent_version;
use crate::session::session::Session;
use crate::session::startup::SessionStartup;
use crate::session::startup::SessionStartupGuard;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use crate::thread_startup_metadata::ThreadStartupMetadata;
use codex_agent_graph_store::AgentGraphStore;
use codex_agent_graph_store::LocalAgentGraphStore;
use codex_agent_message_board_extension::local_board_options;
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::TurnStatus;
use codex_attachment_store::AttachmentStore;
use codex_attachment_store::InlineAttachmentStore;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::DisabledCodeModeSessionProvider;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_core_plugins::PluginsManager;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::ThreadInstructionsProvider;
use codex_extension_api::UserInstructionsProvider;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_history::HistoryInitialization;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_install_context::InstallContext;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR;
use codex_login::default_client::originator;
use codex_message_board::LocalAgentMessageBoard;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::mcp::OPENAI_STANDARD_FORM_INPUT_EXTENSION_ID;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnEnvironmentRequest;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::W3cTraceContext;
use codex_rollout::state_db::StateDbHandle;
use codex_skills_extension::HostSkillsService;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::MoveThreadToSectionParams;
use codex_thread_store::PreparedFork;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::StoredModelContext;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadMetadataPatch;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreResumePreparation;
use codex_thread_store::UpdateThreadMetadataParams;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_git_discovery::GitRootDiscovery;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::instrument;
use tracing::warn;

const THREAD_CREATED_CHANNEL_CAPACITY: usize = 1024;

/// Test-only override for enabling thread-manager behaviors used by integration
/// tests.
///
/// In production builds this value should remain at its default (`false`) and
/// must not be toggled.
static FORCE_TEST_THREAD_MANAGER_BEHAVIOR: AtomicBool = AtomicBool::new(false);

type CapturedOps = Vec<(ThreadId, Op)>;
type SharedCapturedOps = Arc<std::sync::Mutex<CapturedOps>>;
pub(crate) type ThreadIdGenerator = Arc<dyn Fn() -> ThreadId + Send + Sync>;
type AgentControlFactory =
    Arc<dyn Fn(ThreadId) -> BoxFuture<'static, CodexResult<Arc<dyn AgentControl>>> + Send + Sync>;

// `Op` is intentionally not `Clone`. Thread-manager tests only snapshot the
// small subset of ops they inspect.
fn capture_test_op(op: &Op) -> Option<Op> {
    match op {
        Op::Interrupt => Some(Op::Interrupt),
        Op::InterAgentCommunication {
            communication,
            start_options,
        } => Some(Op::InterAgentCommunication {
            communication: communication.clone(),
            start_options: start_options.clone(),
        }),
        Op::Shutdown => Some(Op::Shutdown),
        _ => None,
    }
}

pub(crate) fn default_thread_id_generator() -> ThreadIdGenerator {
    Arc::new(ThreadId::new)
}

pub(crate) fn set_thread_manager_test_mode_for_tests(enabled: bool) {
    FORCE_TEST_THREAD_MANAGER_BEHAVIOR.store(enabled, Ordering::Relaxed);
}

fn should_use_test_thread_manager_behavior() -> bool {
    FORCE_TEST_THREAD_MANAGER_BEHAVIOR.load(Ordering::Relaxed)
}

struct TempCodexHomeGuard {
    path: PathBuf,
}

impl Drop for TempCodexHomeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Represents a newly created Codex thread (formerly called a conversation), including the first event
/// (which is [`EventMsg::SessionConfigured`]).
pub struct NewThread {
    pub thread_id: ThreadId,
    pub thread: Arc<CodexThread>,
    pub session_configured: SessionConfiguredEvent,
}

// TODO(ccunningham): Add an explicit non-interrupting live-turn snapshot once
// core can represent sampling boundaries directly instead of relying on
// whichever items happened to be persisted mid-turn.
//
// Two likely future variants:
// - `TruncateToLastSamplingBoundary` for callers that want a coherent fork from
//   the last stable model boundary without synthesizing an interrupt.
// - `WaitUntilNextSamplingBoundary` (or similar) for callers that prefer to
//   fork after the next sampling boundary rather than interrupting immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkSnapshot {
    /// Fork a committed prefix ending strictly before the nth user message.
    ///
    /// When `n` is within range, this cuts before that 0-based user-message
    /// boundary. When `n` is out of range and the source thread is currently
    /// mid-turn, this instead cuts before the active turn's opening boundary
    /// so the fork drops the unfinished turn suffix. When `n` is out of range
    /// and the source thread is already at a turn boundary, this returns the
    /// full committed history unchanged.
    TruncateBeforeNthUserMessage(usize),

    /// Fork the current persisted history as if the source thread had been
    /// interrupted now.
    ///
    /// If the persisted snapshot ends mid-turn, this appends the same
    /// `<turn_aborted>` marker produced by a real interrupt. If the snapshot is
    /// already at a turn boundary, this returns the current persisted history
    /// unchanged.
    Interrupted,
}

struct ForkHistory {
    snapshot: ForkSnapshot,
    initial_history: InitialHistory,
    persistence: ForkPersistence,
}

/// Preserve legacy `fork_legacy_thread(usize, ...)` callsites by mapping them to the
/// existing truncate-before-nth-user-message snapshot mode.
impl From<usize> for ForkSnapshot {
    fn from(value: usize) -> Self {
        Self::TruncateBeforeNthUserMessage(value)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThreadShutdownReport {
    pub completed: Vec<ThreadId>,
    pub submit_failed: Vec<ThreadId>,
    pub timed_out: Vec<ThreadId>,
}

enum ShutdownOutcome {
    Complete,
    SubmitFailed,
    TimedOut,
}

/// [`ThreadManager`] is responsible for creating threads and maintaining
/// them in memory.
pub struct ThreadManager {
    state: Arc<ThreadManagerState>,
    _test_codex_home_guard: Option<TempCodexHomeGuard>,
}

/// Captured parent identity for an internal child, including inline parents that
/// do not have a public entry in the thread manager. Authentication and the shared
/// agent budget remain bound to the parent that created this value.
#[derive(Clone)]
pub struct InternalSessionParent {
    pub(crate) thread_id: ThreadId,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) agent_control: AgentControlInit,
    pub(crate) originator: String,
    pub(crate) inherited_instructions: Option<SessionInstructions>,
}

/// Controls the prompt cache key sent on model requests, independently of session identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PromptCacheKey {
    /// Preserve the existing session-derived key, including internal-session and fork routing.
    #[default]
    SessionDerived,
    /// Send the caller's key on every model request in the session.
    Custom(String),
    /// Leave `prompt_cache_key` out of model requests.
    Omit,
}

pub struct StartThreadOptions {
    pub config: Config,
    /// Request-body cache key policy for this thread and its children. Hosts must reapply it on cold resume.
    pub prompt_cache_key: PromptCacheKey,
    /// Host-owned provider for this root thread's additional instructions.
    ///
    /// Core composes these after the global [`UserInstructionsProvider`]
    /// snapshot and before repository instructions. `None` or blank provider
    /// output clears only the thread contribution. Core retains this provider
    /// and reads it when capturing model-request context. Hosts own fetching
    /// and caching, and must supply a provider again when cold-resuming a thread
    /// or forking an offline source. A live source can supply its applied snapshot
    /// to a fork, but its task-bound provider is never inherited by the new root.
    /// Registration lasts for the live session. Warm resume ignores this option
    /// and reuses the existing provider without loading it; normal model-request
    /// boundaries load its latest instructions.
    pub thread_instructions_provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    pub allow_provider_model_fallback: bool,
    pub initial_history: InitialHistory,
    /// Store-owned state prepared while loading the resumed history.
    pub resume_preparation: Option<ThreadStoreResumePreparation>,
    /// Parent captured for `start_thread`, including a parent not in the live registry.
    pub internal_parent: Option<InternalSessionParent>,
    pub history_mode: Option<ThreadHistoryMode>,
    pub session_source: Option<SessionSource>,
    pub thread_source: Option<ThreadSource>,
    pub dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
    pub metrics_service_name: Option<String>,
    pub parent_trace: Option<W3cTraceContext>,
    pub environments: Option<Vec<TurnEnvironmentRequest>>,
    /// Existing environment bindings captured by an internal caller.
    pub inherited_environments: Option<TurnEnvironmentSnapshot>,
    /// Explicit global instructions carried by an internal caller instead of loading them again.
    pub user_instructions: Option<LoadedUserInstructions>,
    pub thread_extension_init: ExtensionDataInit,
    /// Host-supplied data captured by each new turn. This is not persisted.
    pub turn_extension_init: ExtensionDataInit,
    pub client_mcp_extensions: ClientMcpExtensions,
    /// Thread ID reserved before startup so the caller can associate host-owned state with it.
    pub reserved_thread_id: Option<ThreadId>,
    /// Initial thread-owned plugin selection; omission restores persisted settings.
    pub disabled_plugin_ids: Option<Vec<String>>,
}

impl StartThreadOptions {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            prompt_cache_key: PromptCacheKey::default(),
            thread_instructions_provider: None,
            allow_provider_model_fallback: false,
            initial_history: InitialHistory::New,
            resume_preparation: None,
            internal_parent: None,
            history_mode: None,
            session_source: None,
            thread_source: None,
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            parent_trace: None,
            environments: None,
            inherited_environments: None,
            user_instructions: None,
            thread_extension_init: ExtensionDataInit::default(),
            turn_extension_init: ExtensionDataInit::default(),
            client_mcp_extensions: ClientMcpExtensions::default(),
            reserved_thread_id: None,
            disabled_plugin_ids: None,
        }
    }
}

struct ThreadSpawnRequest {
    delegated_launch: bool,
    native_admission: Option<crate::agent::child_provider_authorization::NativeChildAdmission>,
    provider_origin: Option<codex_config::child_provider::VerifiedChildProviderOrigin>,
    startup: Option<Arc<crate::session::startup::SessionStartup>>,
    options: StartThreadOptions,
    auth_manager: Arc<AuthManager>,
    agent_control: AgentControlInit,
    parent_thread_id: Option<ThreadId>,
    parent_originator: Option<String>,
    forked_from_thread_id: Option<ThreadId>,
    fork_persistence: ForkPersistence,
    inherited_environments: Option<TurnEnvironmentSnapshot>,
    inherited_instructions: Option<SessionInstructions>,
    inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
    user_shell_override: Option<crate::shell::Shell>,
}

impl ThreadSpawnRequest {
    fn new(
        options: StartThreadOptions,
        auth_manager: Arc<AuthManager>,
        agent_control: impl Into<AgentControlInit>,
    ) -> Self {
        Self {
            delegated_launch: false,
            native_admission: None,
            provider_origin: None,
            startup: None,
            options,
            auth_manager,
            agent_control: agent_control.into(),
            parent_thread_id: None,
            parent_originator: None,
            forked_from_thread_id: None,
            fork_persistence: ForkPersistence::Copied,
            inherited_environments: None,
            inherited_instructions: None,
            inherited_exec_policy: None,
            user_shell_override: None,
        }
    }
}

fn originator_from_service_name(service_name: Option<&str>) -> Option<String> {
    let service_name = service_name?.trim();
    for originator in [
        "codex_work_desktop",
        "codex_work_web",
        "codex_work_mobile",
        "codex_work_cca",
        "chatgpt_cca",
    ] {
        if service_name.eq_ignore_ascii_case(originator) {
            return Some(originator.to_string());
        }
    }
    None
}

fn effective_originator_value(
    metrics_service_name: Option<&str>,
    env_originator: Option<String>,
    persisted_originator: Option<String>,
    inherited_originator: Option<String>,
    default_originator: String,
) -> String {
    originator_from_service_name(metrics_service_name)
        .or(persisted_originator)
        .or(inherited_originator)
        .or(env_originator)
        .unwrap_or(default_originator)
}

pub(crate) struct ResumeThreadWithHistoryOptions {
    pub(crate) authorization: Option<crate::agent::child_provider_authorization::ChildResumePermit>,
    pub(crate) config: Config,
    pub(crate) prompt_cache_key: Option<PromptCacheKey>,
    pub(crate) initial_history: InitialHistory,
    pub(crate) agent_control: LocalAgentControl,
    pub(crate) session_source: SessionSource,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) environment_selections: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) inherited_environments: Option<TurnEnvironmentSnapshot>,
    pub(crate) inherited_instructions: Option<SessionInstructions>,
    pub(crate) inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
    pub(crate) client_mcp_extensions: Option<ClientMcpExtensions>,
}

/// Captured under one manager write/revoke operation. The root Arc/absence
/// cannot be supplied by a caller or reconstructed from history/config.
pub(crate) struct NativeShutdownCapture {
    pub(crate) threads: Vec<(ThreadId, Arc<CodexThread>)>,
    root: ThreadId,
    root_allocation: Option<Arc<CodexThread>>,
    graph_stamp: crate::agent::NativeGraphCloseStamp,
}

/// Shared, `Arc`-owned state for [`ThreadManager`]. This `Arc` is required to have a single
/// `Arc` reference that can be downgraded to by `LocalAgentControl` while preventing every single
/// function to require an `Arc<&Self>`.
pub(crate) struct ThreadManagerState {
    // Eviction updates this registry and residency together, locking the registry first.
    pub(crate) threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>,
    shared_thread_instructions: shared_instructions::SharedThreadInstructionsProviders,
    thread_created_tx: broadcast::Sender<ThreadId>,
    thread_id_generator: ThreadIdGenerator,
    agent_control_factory: Option<AgentControlFactory>,
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
    git_root_discovery: Arc<GitRootDiscovery>,
    environment_manager: Arc<EnvironmentManager>,
    starting_mcp_runtimes: std::sync::Mutex<Vec<std::sync::Weak<AtomicBool>>>,
    skills_service: Arc<HostSkillsService>,
    plugins_manager: Arc<PluginsManager>,
    mcp_manager: Arc<McpManager>,
    code_mode_session_provider: Arc<dyn CodeModeSessionProvider>,
    pub(crate) extensions: Arc<ExtensionRegistry<Config>>,
    user_instructions_provider: Arc<dyn UserInstructionsProvider>,
    image_store: Arc<dyn AttachmentStore>,
    thread_store: Arc<dyn ThreadStore>,
    agent_graph_store: Option<Arc<dyn AgentGraphStore>>,
    attestation_provider: Option<Arc<dyn AttestationProvider>>,
    external_time_provider: Option<Arc<dyn TimeProvider>>,
    session_source: SessionSource,
    installation_id: String,
    analytics_events_client: Option<AnalyticsEventsClient>,
    // Captures submitted ops for testing purpose when test mode is enabled.
    ops_log: Option<SharedCapturedOps>,
}

pub fn build_models_manager(
    config: &Config,
    auth_manager: Arc<AuthManager>,
) -> SharedModelsManager {
    let provider = create_model_provider(config.model_provider.clone(), Some(auth_manager));
    let manager = provider.models_manager(
        config.codex_home.to_path_buf(),
        config.model_catalog.clone(),
    );
    manager.set_api_key_model_discovery_enabled(
        config.features.enabled(Feature::ApiKeyModelDiscovery),
    );
    manager
}

pub fn thread_store_from_config(
    config: &Config,
    state_db: Option<StateDbHandle>,
) -> Arc<dyn ThreadStore> {
    match &config.experimental_thread_store {
        ThreadStoreConfig::Local => {
            let compression_enabled = config
                .features
                .enabled(Feature::LocalThreadStoreCompression);
            let background_migration_enabled = config
                .features
                .enabled(Feature::BackgroundPaginatedRolloutMigration);
            let has_state_db = state_db.is_some();
            let sqlite = config.sqlite_config().clone();
            let store = Arc::new(
                LocalThreadStore::new(LocalThreadStoreConfig::from_config(config), state_db)
                    .with_thread_data_cleanup(move |thread_ids| {
                        let sqlite = sqlite.clone();
                        Box::pin(async move {
                            let boards = thread_ids.into_iter().map(Into::into).collect::<Vec<_>>();
                            LocalAgentMessageBoard::delete_boards(
                                &sqlite,
                                &local_board_options(),
                                &boards,
                            )
                            .await
                            .map_err(|err| {
                                ThreadStoreError::Internal {
                                    message: format!(
                                        "failed to delete agent message boards: {err}"
                                    ),
                                }
                            })
                        })
                    }),
            );
            if has_state_db && background_migration_enabled {
                let startup_store = Arc::clone(&store);
                let codex_home = config.codex_home.to_path_buf();
                tokio::spawn(async move {
                    if let Err(err) = startup_store.migrate_rollouts_on_startup().await {
                        warn!("failed to migrate legacy rollouts on startup: {err}");
                    }
                    if compression_enabled {
                        codex_rollout::spawn_rollout_compression_worker(
                            codex_home,
                            codex_rollout::RolloutCompressionTrigger::Startup,
                        );
                    }
                });
            } else if compression_enabled {
                codex_rollout::spawn_rollout_compression_worker(
                    config.codex_home.to_path_buf(),
                    codex_rollout::RolloutCompressionTrigger::Startup,
                );
            }
            store
        }
        ThreadStoreConfig::InMemory { id } => {
            Arc::new(InMemoryThreadStore::for_id(id).with_state_db(state_db))
        }
    }
}

/// Constructs the default image store that preserves inline images.
pub fn passthrough_image_store() -> Arc<dyn AttachmentStore> {
    Arc::new(InlineAttachmentStore)
}

/// Construct the default SQLite-backed agent graph store when local state is available.
pub fn local_agent_graph_store_from_state_db(
    state_db: Option<&StateDbHandle>,
) -> Option<Arc<dyn AgentGraphStore>> {
    state_db.map(|state_db| {
        Arc::new(LocalAgentGraphStore::new(Arc::clone(state_db))) as Arc<dyn AgentGraphStore>
    })
}

impl ThreadManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Config,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        codex_apps_tools_cache: CodexAppsToolsCache,
        session_source: SessionSource,
        environment_manager: Arc<EnvironmentManager>,
        extensions: Arc<ExtensionRegistry<Config>>,
        user_instructions_provider: Arc<dyn UserInstructionsProvider>,
        analytics_events_client: Option<AnalyticsEventsClient>,
        image_store: Arc<dyn AttachmentStore>,
        thread_store: Arc<dyn ThreadStore>,
        agent_graph_store: Option<Arc<dyn AgentGraphStore>>,
        installation_id: String,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
    ) -> Self {
        let codex_home = config.codex_home.clone();
        let restriction_product = session_source.restriction_product();
        let (thread_created_tx, _) = broadcast::channel(THREAD_CREATED_CHANNEL_CAPACITY);
        let skills_service = Arc::new(HostSkillsService::new_with_restriction_product(
            codex_home.clone(),
            config.bundled_skills_enabled(),
            restriction_product,
        ));
        let plugins_manager = Arc::new(PluginsManager::new_with_options(
            codex_home.to_path_buf(),
            restriction_product,
            Arc::clone(&auth_manager),
            skills_service.clone(),
        ));
        let mcp_manager = Arc::new(McpManager::new_with_extensions(
            Arc::clone(&plugins_manager),
            Arc::clone(&extensions),
            codex_apps_tools_cache,
        ));
        let code_mode_session_provider: Arc<dyn CodeModeSessionProvider> =
            if config.features.enabled(Feature::CodeModeHost)
                || config.code_mode.disable_in_process_fallback
            {
                Self::process_code_mode_session_provider(
                    config,
                    InstallContext::current().code_mode_host_program(),
                )
            } else {
                Arc::new(DisabledCodeModeSessionProvider)
            };
        Self {
            state: Arc::new(ThreadManagerState {
                threads: Arc::new(RwLock::new(HashMap::new())),
                shared_thread_instructions: Default::default(),
                thread_created_tx,
                thread_id_generator: default_thread_id_generator(),
                agent_control_factory: None,
                models_manager,
                git_root_discovery: Arc::default(),
                environment_manager,
                starting_mcp_runtimes: std::sync::Mutex::new(Vec::new()),
                skills_service,
                plugins_manager,
                mcp_manager,
                code_mode_session_provider,
                extensions,
                user_instructions_provider,
                image_store,
                thread_store,
                agent_graph_store,
                attestation_provider,
                external_time_provider,
                auth_manager,
                session_source,
                installation_id,
                analytics_events_client,
                ops_log: should_use_test_thread_manager_behavior()
                    .then(|| Arc::new(std::sync::Mutex::new(Vec::new()))),
            }),
            _test_codex_home_guard: None,
        }
    }

    /// Generate every new thread identifier with the caller-provided factory.
    pub fn with_thread_id_generator(
        mut self,
        generator: impl Fn() -> ThreadId + Send + Sync + 'static,
    ) -> Self {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("thread ID generator must be set before thread manager is shared");
        };
        state.thread_id_generator = Arc::new(generator);
        self
    }

    /// Selects the host-owned agent controller for every new or cold-resumed thread.
    ///
    /// The factory receives the thread's final ID and must return the controller for
    /// its agent tree. New roots use their thread ID as the controller identity;
    /// resumed threads must match their persisted session identity. Factory errors
    /// fail startup. Internal sessions inherit their parent's controller, and warm
    /// resume retains the live controller without calling the factory.
    ///
    /// Install this before starting threads. Without a factory, the manager uses
    /// the local backend. Host backends support MultiAgentV2; V1 requires local control.
    pub fn with_agent_control_factory<F, Fut>(mut self, factory: F) -> Self
    where
        F: Fn(ThreadId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CodexResult<Arc<dyn AgentControl>>> + Send + 'static,
    {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("agent controller factory must be set before thread manager is shared");
        };
        state.agent_control_factory = Some(Arc::new(move |thread_id| Box::pin(factory(thread_id))));
        self
    }

    /// Replaces the process-wide provider before this manager is shared with threads.
    pub fn with_code_mode_session_provider(
        mut self,
        provider: Arc<dyn CodeModeSessionProvider>,
    ) -> Self {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("code-mode session provider must be set before thread manager is shared");
        };
        state.code_mode_session_provider = provider;
        self
    }

    pub(crate) fn with_code_mode_host_program_for_tests(
        mut self,
        host_program: PathBuf,
        config: &Config,
    ) -> Self {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("new thread manager state should not be shared");
        };
        state.code_mode_session_provider =
            Self::process_code_mode_session_provider(config, host_program);
        self
    }

    fn process_code_mode_session_provider(
        config: &Config,
        host_program: PathBuf,
    ) -> Arc<dyn CodeModeSessionProvider> {
        if config.features.enabled(Feature::CodeModeHostGrpc) {
            Arc::new(
                crate::code_mode_host::ProcessOwnedGrpcCodeModeSessionProvider::with_host_program(
                    host_program,
                ),
            )
        } else {
            Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
                host_program,
            ))
        }
    }

    /// Construct with a dummy AuthManager containing the provided CodexAuth.
    /// Used for integration tests: should not be used by ordinary business logic.
    pub(crate) fn with_models_provider_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
    ) -> Self {
        set_thread_manager_test_mode_for_tests(/*enabled*/ true);
        let codex_home = std::env::temp_dir().join(format!(
            "codex-thread-manager-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&codex_home)
            .unwrap_or_else(|err| panic!("temp codex home dir create failed: {err}"));
        let mut manager = Self::with_models_provider_and_home_for_tests(
            auth,
            provider,
            codex_home.clone(),
            Arc::new(EnvironmentManager::default_for_tests()),
        );
        manager._test_codex_home_guard = Some(TempCodexHomeGuard { path: codex_home });
        manager
    }

    /// Construct with a dummy AuthManager containing the provided CodexAuth and codex home.
    /// Used for integration tests: should not be used by ordinary business logic.
    pub(crate) fn with_models_provider_and_home_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
        codex_home: PathBuf,
        environment_manager: Arc<EnvironmentManager>,
    ) -> Self {
        Self::with_models_provider_home_and_state_for_tests(
            auth,
            provider,
            codex_home,
            environment_manager,
            /*state_db*/ None,
        )
    }

    pub(crate) fn with_models_provider_home_and_state_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
        codex_home: PathBuf,
        environment_manager: Arc<EnvironmentManager>,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        set_thread_manager_test_mode_for_tests(/*enabled*/ true);
        let auth_manager = AuthManager::from_auth_for_testing(auth);
        let installation_id = uuid::Uuid::new_v4().to_string();
        let absolute_codex_home = match AbsolutePathBuf::from_absolute_path_checked(&codex_home) {
            Ok(codex_home) => codex_home,
            Err(err) => panic!("test codex_home should be absolute: {err}"),
        };
        let (thread_created_tx, _) = broadcast::channel(THREAD_CREATED_CHANNEL_CAPACITY);
        let restriction_product = SessionSource::Exec.restriction_product();
        let skills_service = Arc::new(HostSkillsService::new_with_restriction_product(
            absolute_codex_home.clone(),
            /*bundled_skills_enabled*/ true,
            restriction_product,
        ));
        let plugins_manager = Arc::new(PluginsManager::new_with_options(
            codex_home.clone(),
            restriction_product,
            Arc::clone(&auth_manager),
            skills_service.clone(),
        ));
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&plugins_manager)));
        // This test constructor has no Config input. Tests that need a non-local
        // process store should construct ThreadManager::new with an explicit store.
        let thread_store: Arc<dyn ThreadStore> = Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: codex_home.clone(),
                sqlite: codex_state::SqliteConfig::new_for_testing(absolute_codex_home),
                default_model_provider_id: OPENAI_PROVIDER_ID.to_string(),
            },
            state_db.clone(),
        ));
        let agent_graph_store = local_agent_graph_store_from_state_db(state_db.as_ref());
        Self {
            state: Arc::new(ThreadManagerState {
                threads: Arc::new(RwLock::new(HashMap::new())),
                shared_thread_instructions: Default::default(),
                thread_created_tx,
                thread_id_generator: default_thread_id_generator(),
                agent_control_factory: None,
                models_manager: create_model_provider(provider, Some(auth_manager.clone()))
                    .models_manager(codex_home, /*config_model_catalog*/ None),
                git_root_discovery: Arc::default(),
                environment_manager,
                starting_mcp_runtimes: std::sync::Mutex::new(Vec::new()),
                skills_service,
                plugins_manager,
                mcp_manager,
                code_mode_session_provider: Arc::new(DisabledCodeModeSessionProvider),
                extensions: empty_extension_registry(),
                user_instructions_provider: Arc::new(
                    crate::test_support::EmptyUserInstructionsProvider,
                ),
                image_store: passthrough_image_store(),
                thread_store,
                agent_graph_store,
                attestation_provider: None,
                external_time_provider: None,
                auth_manager,
                session_source: SessionSource::Exec,
                installation_id,
                analytics_events_client: None,
                ops_log: should_use_test_thread_manager_behavior()
                    .then(|| Arc::new(std::sync::Mutex::new(Vec::new()))),
            }),
            _test_codex_home_guard: None,
        }
    }

    pub fn session_source(&self) -> SessionSource {
        self.state.session_source.clone()
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.state.auth_manager.clone()
    }

    pub fn skills_service(&self) -> Arc<HostSkillsService> {
        self.state.skills_service.clone()
    }

    pub fn plugins_manager(&self) -> Arc<PluginsManager> {
        self.state.plugins_manager.clone()
    }

    pub fn mcp_manager(&self) -> Arc<McpManager> {
        self.state.mcp_manager.clone()
    }

    pub fn environment_manager(&self) -> Arc<EnvironmentManager> {
        self.state.environment_manager.clone()
    }

    pub fn image_store(&self) -> Arc<dyn AttachmentStore> {
        Arc::clone(&self.state.image_store)
    }

    /// Starts the local rollout migration path after a runtime feature enablement.
    ///
    /// Startup config handles the initial launch in [`thread_store_from_config`]. This covers
    /// clients that decide to enable background migration after constructing the app-server.
    pub fn start_background_rollout_migration(&self) {
        let Some(store) = self
            .state
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        else {
            return;
        };
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(err) = store.migrate_rollouts_on_startup().await {
                warn!("failed to migrate legacy rollouts on startup: {err}");
            }
        });
    }

    /// Refreshes every loaded thread and marks threads that are still being created.
    pub async fn invalidate_mcp_runtimes(&self) {
        self.invalidate_starting_mcp_runtimes();
        let threads = self
            .state
            .threads
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for thread in threads {
            thread.session.request_mcp_runtime_refresh();
        }
    }

    /// Rebuilds loaded hook runtimes without reloading their session configurations.
    pub async fn refresh_hook_runtimes(&self) {
        let threads = self
            .state
            .threads
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for thread in threads {
            let config = thread.session.get_config().await;
            thread.session.refresh_hooks(config).await;
        }
    }

    fn invalidate_starting_mcp_runtimes(&self) {
        let mut starting = self
            .state
            .starting_mcp_runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        starting.retain(|runtime| {
            let Some(runtime) = runtime.upgrade() else {
                return false;
            };
            runtime.store(true, Ordering::Release);
            true
        });
    }

    pub fn default_environment_requests(
        &self,
        cwd: &AbsolutePathBuf,
        workspace_roots: &[AbsolutePathBuf],
    ) -> Vec<TurnEnvironmentRequest> {
        default_thread_environment_requests(
            self.state.environment_manager.as_ref(),
            cwd,
            workspace_roots,
        )
    }

    pub(crate) fn git_root_discovery(&self) -> Arc<GitRootDiscovery> {
        Arc::clone(&self.state.git_root_discovery)
    }

    pub fn get_models_manager(&self) -> SharedModelsManager {
        self.state.models_manager.clone()
    }

    pub async fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Vec<ModelPreset> {
        self.state
            .models_manager
            .list_models(refresh_strategy, http_client_factory)
            .await
    }

    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.state.models_manager.list_collaboration_modes()
    }

    pub async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.state.list_thread_ids().await
    }

    pub fn subscribe_thread_created(&self) -> broadcast::Receiver<ThreadId> {
        self.state.thread_created_tx.subscribe()
    }

    pub async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        self.state.get_thread(thread_id).await
    }

    /// Board hosts may reach internal runtimes only through a matching explicit connection.
    pub(crate) async fn get_thread_for_board(
        &self,
        thread_id: ThreadId,
        connection: &codex_message_board::LocalBoardConnection,
    ) -> CodexResult<Arc<CodexThread>> {
        self.state
            .threads
            .read()
            .await
            .get(&thread_id)
            .filter(|thread| {
                thread
                    .thread_extension_data()
                    .get::<codex_message_board::LocalBoardConnection>()
                    .is_some_and(|other| connection.same_board(&other))
            })
            .cloned()
            .ok_or(CodexErr::ThreadNotFound(thread_id))
    }

    /// Updates metadata for loaded and cold threads through one entrypoint.
    ///
    /// Loaded threads route through `CodexThread`/`LiveThread`, so metadata changes stay ordered
    /// with live rollout writes. Cold threads go directly to the store, which owns unloaded JSONL
    /// compatibility and SQLite metadata updates. This API always returns a materialized thread;
    /// if the store reports a successful no-op without one, it performs a fallback read.
    pub async fn update_thread_metadata(
        &self,
        thread_id: ThreadId,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> CodexResult<StoredThread> {
        if let Ok(thread) = self.get_thread(thread_id).await {
            if thread.config_snapshot().await.ephemeral {
                return Err(CodexErr::InvalidRequest(format!(
                    "ephemeral thread does not support metadata updates: {thread_id}"
                )));
            }
            return thread
                .update_thread_metadata(patch, include_archived)
                .await
                .map_err(|err| thread_store_metadata_update_error(thread_id, err));
        }
        let updated = self
            .state
            .thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id,
                patch,
                include_archived,
            })
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                err => thread_store_metadata_update_error(thread_id, err),
            })?;
        match updated {
            Some(thread) => Ok(thread),
            None => self
                .state
                .thread_store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived,
                    include_history: false,
                })
                .await
                .map_err(|err| thread_store_metadata_update_error(thread_id, err)),
        }
    }

    /// Moves a thread to, within, or out of a server-ordered section.
    pub async fn move_thread_to_section(
        &self,
        thread_id: ThreadId,
        section: Option<&str>,
        before_thread_id: Option<ThreadId>,
    ) -> CodexResult<()> {
        if let Ok(thread) = self.get_thread(thread_id).await {
            if thread.config_snapshot().await.ephemeral {
                return Err(CodexErr::InvalidRequest(format!(
                    "ephemeral thread does not support section moves: {thread_id}"
                )));
            }
            // Explicit placement must work before the first turn materializes the thread.
            thread.ensure_rollout_materialized().await;
            thread.flush_rollout().await?;
        }

        self.state
            .thread_store
            .move_thread_to_section(MoveThreadToSectionParams {
                thread_id,
                section: section.map(ToOwned::to_owned),
                before_thread_id,
            })
            .await
            .map_err(|err| thread_store_metadata_update_error(thread_id, err))
    }

    /// List `thread_id` plus all known descendants in its spawn subtree.
    pub async fn list_agent_subtree_thread_ids(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut subtree_thread_ids = Vec::new();
        let mut seen_thread_ids = HashSet::new();
        subtree_thread_ids.push(thread_id);
        seen_thread_ids.insert(thread_id);

        if let Some(agent_graph_store) = self.state.agent_graph_store() {
            for descendant_id in agent_graph_store
                .list_thread_spawn_descendants(thread_id, /*status_filter*/ None)
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!("failed to load thread-spawn descendants: {err}"))
                })?
            {
                if seen_thread_ids.insert(descendant_id) {
                    subtree_thread_ids.push(descendant_id);
                }
            }
        }

        for descendant_id in self
            .agent_control()
            .runtime
            .list_live_agent_subtree_thread_ids(thread_id)
            .await?
        {
            if seen_thread_ids.insert(descendant_id) {
                subtree_thread_ids.push(descendant_id);
            }
        }

        Ok(subtree_thread_ids)
    }

    #[cfg(test)]
    pub(crate) async fn start_thread_with_control_for_tests(
        &self,
        options: StartThreadOptions,
        control: LocalAgentControl,
    ) -> CodexResult<NewThread> {
        self.state
            .start_root_with_control_for_tests(options, control)
            .await
    }

    pub async fn start_thread(&self, options: StartThreadOptions) -> CodexResult<NewThread> {
        Box::pin(self.start_thread_inner(
            options, /*forked_from_thread_id*/ None, /*startup*/ None,
        ))
        .await
    }

    /// Starts a fresh internal session associated with an existing parent thread.
    pub async fn spawn_internal_session(
        &self,
        parent_thread_id: ThreadId,
        options: StartThreadOptions,
    ) -> CodexResult<NewThread> {
        self.spawn_internal_session_with_history(
            parent_thread_id,
            options,
            InitialHistory::New,
            /*inherited_instructions*/ None,
        )
        .await
    }

    /// Starts an internal session from an explicitly selected, committed history.
    ///
    /// The caller selects the history; this never reads an in-flight parent turn or
    /// appends an interruption marker. Authentication and budget still come from the parent.
    pub async fn fork_internal_session(
        &self,
        parent_thread_id: ThreadId,
        options: StartThreadOptions,
        history: Vec<RolloutItem>,
    ) -> CodexResult<NewThread> {
        self.spawn_internal_session_with_history(
            parent_thread_id,
            options,
            InitialHistory::Forked(history),
            /*inherited_instructions*/ None,
        )
        .await
    }

    pub(crate) async fn spawn_internal_session_with_history(
        &self,
        parent_thread_id: ThreadId,
        mut options: StartThreadOptions,
        history: InitialHistory,
        inherited_instructions: Option<SessionInstructions>,
    ) -> CodexResult<NewThread> {
        if options.config.prepared_child_authorization.is_some()
            || !matches!(options.session_source, Some(SessionSource::Internal(_)))
        {
            return Err(CodexErr::InvalidRequest(
                "internal sessions require an internal session source".to_string(),
            ));
        }
        let parent = self.get_thread(parent_thread_id).await?;
        options.initial_history = history;
        options.internal_parent = Some(InternalSessionParent {
            thread_id: parent_thread_id,
            auth_manager: Arc::clone(&parent.session.services.auth_manager),
            agent_control: AgentControlInit::Provided {
                control: Arc::clone(&parent.session.services.agent_control),
                runtime: parent.session.services.local_agent_runtime.clone(),
            },
            originator: parent.config_snapshot().await.originator,
            inherited_instructions,
        });
        self.start_thread(options).await
    }

    /// Allocates a thread ID before startup so a caller can associate host-owned state with it.
    pub fn reserve_thread_id(&self) -> ThreadId {
        self.state.thread_id_generator.as_ref()()
    }

    async fn start_thread_inner(
        &self,
        mut options: StartThreadOptions,
        forked_from_thread_id: Option<ThreadId>,
        startup: Option<Arc<crate::session::startup::SessionStartup>>,
    ) -> CodexResult<NewThread> {
        let (resumed_session_source, resumed_thread_source) = options
            .initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        options.session_source = Some(
            options
                .session_source
                .take()
                .unwrap_or(resumed_session_source),
        );
        options.thread_source = options.thread_source.take().or(resumed_thread_source);
        let parent = options.internal_parent.take();
        if parent.is_some()
            && (!matches!(options.session_source, Some(SessionSource::Internal(_)))
                || matches!(options.initial_history, InitialHistory::Resumed(_)))
        {
            return Err(CodexErr::InvalidRequest(
                "a captured internal parent requires a new or forked internal session".to_owned(),
            ));
        }
        let mut request = if let Some(parent) = parent {
            let mut request =
                ThreadSpawnRequest::new(options, parent.auth_manager, parent.agent_control);
            request.parent_thread_id = Some(parent.thread_id);
            request.parent_originator = Some(parent.originator);
            request.inherited_instructions = parent.inherited_instructions;
            request.forked_from_thread_id = request.options.initial_history.forked_from_id();
            request
        } else {
            let agent_control = self.agent_control_for_config(&options.config);
            let mut request = ThreadSpawnRequest::new(
                options,
                Arc::clone(&self.state.auth_manager),
                agent_control,
            );
            request.forked_from_thread_id = forked_from_thread_id;
            request
        };
        request.startup = startup;
        Box::pin(self.state.spawn_thread(request)).await
    }

    // TODO(jif) merge with fork_agent
    /// Spawn a subagent from a legacy thread's full persisted history.
    ///
    /// Paginated sources are rejected. For paginated thread forks, use
    /// [`ThreadStore::prepare_fork`] and [`Self::fork_prepared_thread`].
    pub async fn spawn_legacy_subagent(
        &self,
        forked_from_thread_id: ThreadId,
        mut options: StartThreadOptions,
    ) -> CodexResult<NewThread> {
        if options.config.prepared_child_authorization.is_some()
            || matches!(
                options.session_source,
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }))
            )
        {
            return Err(crate::agent::child_provider_authorization::invalid(
                "host legacy review does not accept delegated launch preferences",
            ));
        }
        let fork_source = self.get_thread(forked_from_thread_id).await?;
        // Persist queued rollout updates before reading the fork snapshot.
        fork_source.ensure_rollout_materialized().await;
        fork_source.flush_rollout().await?;
        let stored_thread = fork_source
            .read_thread(
                /*include_archived*/ true, /*include_history*/ true,
            )
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to read subagent fork source {forked_from_thread_id}: {err}"
                ))
            })?;
        let history = stored_thread_to_initial_history(stored_thread, fork_source.rollout_path())?;
        let inherited_multi_agent_version = fork_source
            .multi_agent_version()
            .unwrap_or(MultiAgentVersion::V1);
        options.initial_history = fork_history_from_snapshot(
            ForkSnapshot::Interrupted,
            history,
            InterruptedTurnHistoryMarker::from_config_and_version(
                &options.config,
                inherited_multi_agent_version,
            ),
        );
        self.start_thread_inner(options, Some(forked_from_thread_id), /*startup*/ None)
            .await
    }

    /// Resume a legacy thread by reading its full rollout history.
    ///
    /// Paginated sources are rejected. Load their context with
    /// [`ThreadStore::load_latest_model_context`] and pass it to
    /// [`Self::resume_thread_with_history`] instead.
    pub async fn resume_legacy_thread_from_rollout(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<NewThread> {
        let initial_history = self
            .legacy_initial_history_from_rollout_path(rollout_path)
            .await?;
        Box::pin(self.resume_thread_with_history(
            config,
            initial_history,
            auth_manager,
            parent_trace,
            client_mcp_extensions,
        ))
        .await
    }

    /// Reloads a recorded Multi-Agent V2 child through its currently loaded immediate parent.
    ///
    /// The child keeps the existing parent-controlled reload semantics. Callers cannot supply
    /// configuration overrides, and an unavailable or unrecognized owner is an error.
    pub async fn ensure_multi_agent_v2_child_loaded(
        &self,
        child_thread_id: ThreadId,
    ) -> CodexResult<()> {
        let stored_thread = self
            .state
            .read_stored_thread(ReadThreadParams {
                thread_id: child_thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let Some(parent_thread_id) = stored_thread.parent_thread_id else {
            return Err(CodexErr::InvalidRequest(format!(
                "thread {child_thread_id} is not a recorded multi-agent v2 child"
            )));
        };
        let parent = self.get_thread(parent_thread_id).await.map_err(|_| {
            CodexErr::InvalidRequest(format!(
                "cannot resume multi-agent v2 child {child_thread_id}: parent {parent_thread_id} is not loaded; resume the parent first"
            ))
        })?;
        parent
            .session
            .services
            .agent_control
            .ensure_child_loaded(parent_thread_id, child_thread_id)
            .await
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn resume_thread_with_history(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<NewThread> {
        let agent_control = self.agent_control_for_config(&config);
        let (session_source, thread_source) = initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        let options = StartThreadOptions {
            initial_history,
            session_source: Some(session_source),
            thread_source,
            parent_trace,
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        Box::pin(self.state.spawn_thread(ThreadSpawnRequest::new(
            options,
            auth_manager,
            agent_control,
        )))
        .await
    }

    pub(crate) async fn start_thread_with_user_shell_override_for_tests(
        &self,
        config: Config,
        user_shell_override: crate::shell::Shell,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<NewThread> {
        let agent_control = self.agent_control_for_config(&config);
        let options = StartThreadOptions {
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.state.auth_manager), agent_control);
        request.user_shell_override = Some(user_shell_override);
        Box::pin(self.state.spawn_thread(request)).await
    }

    pub(crate) async fn resume_legacy_thread_from_rollout_with_user_shell_override_for_tests(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
        user_shell_override: crate::shell::Shell,
        client_mcp_extensions: ClientMcpExtensions,
    ) -> CodexResult<NewThread> {
        let agent_control = self.agent_control_for_config(&config);
        let initial_history = self
            .legacy_initial_history_from_rollout_path(rollout_path)
            .await?;
        let (session_source, thread_source) = initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        let options = StartThreadOptions {
            initial_history,
            session_source: Some(session_source),
            thread_source,
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        let mut request = ThreadSpawnRequest::new(options, auth_manager, agent_control);
        request.user_shell_override = Some(user_shell_override);
        Box::pin(self.state.spawn_thread(request)).await
    }

    /// Removes the thread from the manager's internal map, though the thread is stored
    /// as `Arc<CodexThread>`, it is possible that other references to it exist elsewhere.
    /// Returns the thread if the thread was found and removed.
    pub async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        self.state.remove_thread(thread_id).await
    }

    /// Removes a runtime for a client request, leaving internal workers with their owner.
    /// The access check and removal share the same lock so a rejected worker stays registered.
    pub async fn remove_thread_for_client(
        &self,
        thread_id: &ThreadId,
    ) -> CodexResult<Option<Arc<CodexThread>>> {
        let Some(current) = self.state.threads.read().await.get(thread_id).cloned() else {
            return Ok(None);
        };
        if current.session_source.is_internal() {
            return Err(CodexErr::InvalidRequest(
                "live internal threads can only be removed by their owner".into(),
            ));
        }
        let authority =
            crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &current.session,
                &current.session_source,
            )
            .await;
        if let Err(error) = crate::agent::child_provider_authorization::durable::seal_stopped(
            &self.state,
            &current,
            &authority,
        )
        .await
        {
            warn!(%error, "stopped native authority remains unavailable for cold reload");
        }
        let mut threads = self.state.threads.write().await;
        if !threads
            .get(thread_id)
            .is_some_and(|thread| Arc::ptr_eq(thread, &current))
        {
            return Ok(None);
        }
        current.session.revoke_fresh_child_admission();
        current
            .session
            .services
            .local_agent_runtime
            .retire_authority(authority);
        Ok(threads
            .remove(thread_id)
            .inspect(|thread| thread.session.services.mcp_runtime.revoke_ema()))
    }

    /// Removes a thread only if `thread_id` still maps to `expected`.
    ///
    /// Delayed cleanup uses this to avoid removing a replacement runtime registered under the
    /// same thread ID.
    pub async fn remove_thread_if_matches(
        &self,
        thread_id: &ThreadId,
        expected: &Arc<CodexThread>,
    ) -> Option<Arc<CodexThread>> {
        let authority =
            crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &expected.session,
                &expected.session_source,
            )
            .await;
        if let Err(error) = crate::agent::child_provider_authorization::durable::seal_stopped(
            &self.state,
            expected,
            &authority,
        )
        .await
        {
            warn!(%error, "stopped native authority remains unavailable for cold reload");
        }
        let mut threads = self.state.threads.write().await;
        if !threads
            .get(thread_id)
            .is_some_and(|thread| Arc::ptr_eq(thread, expected))
        {
            return None;
        }
        expected.session.revoke_fresh_child_admission();
        expected
            .session
            .services
            .local_agent_runtime
            .retire_authority(authority);
        threads
            .remove(thread_id)
            .inspect(|thread| thread.session.services.mcp_runtime.revoke_ema())
    }

    /// Tries to shut down all tracked threads concurrently within the provided timeout.
    /// Threads that complete shutdown are removed from the manager; incomplete shutdowns
    /// remain tracked so callers can retry or inspect them later. Replacements registered
    /// under a completed target's ID remain running and tracked.
    pub async fn shutdown_all_threads_bounded(&self, timeout: Duration) -> ThreadShutdownReport {
        let threads = {
            let threads = self.state.threads.read().await;
            threads
                .iter()
                .map(|(thread_id, thread)| (*thread_id, Arc::clone(thread)))
                .collect::<Vec<_>>()
        };

        let mut shutdowns = threads
            .into_iter()
            .map(|(thread_id, thread)| async move {
                let outcome = match tokio::time::timeout(timeout, thread.shutdown_and_wait()).await
                {
                    Ok(Ok(())) => ShutdownOutcome::Complete,
                    Ok(Err(_)) => ShutdownOutcome::SubmitFailed,
                    Err(_) => ShutdownOutcome::TimedOut,
                };
                (thread_id, thread, outcome)
            })
            .collect::<FuturesUnordered<_>>();
        let mut report = ThreadShutdownReport::default();
        let mut completed_threads = Vec::new();

        while let Some((thread_id, thread, outcome)) = shutdowns.next().await {
            match outcome {
                ShutdownOutcome::Complete => {
                    report.completed.push(thread_id);
                    completed_threads.push((thread_id, thread));
                }
                ShutdownOutcome::SubmitFailed => report.submit_failed.push(thread_id),
                ShutdownOutcome::TimedOut => report.timed_out.push(thread_id),
            }
        }

        let mut completed_authority = Vec::new();
        for (thread_id, thread) in completed_threads {
            let authority =
                crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                    &thread.session,
                    &thread.session_source,
                )
                .await;
            if let Err(error) = crate::agent::child_provider_authorization::durable::seal_stopped(
                &self.state,
                &thread,
                &authority,
            )
            .await
            {
                warn!(%error, "shutdown native authority remains unavailable for cold reload");
            }
            completed_authority.push((thread_id, thread, authority));
        }
        let mut tracked_threads = self.state.threads.write().await;
        for (thread_id, thread, authority) in completed_authority {
            if !tracked_threads
                .get(&thread_id)
                .is_some_and(|current| Arc::ptr_eq(current, &thread))
            {
                continue;
            }
            thread.session.revoke_fresh_child_admission();
            thread
                .session
                .services
                .local_agent_runtime
                .retire_authority(authority);
            if let Some(removed) = tracked_threads.remove(&thread_id) {
                removed.session.services.mcp_runtime.revoke_ema();
            }
        }

        report
            .completed
            .sort_by_key(std::string::ToString::to_string);
        report
            .submit_failed
            .sort_by_key(std::string::ToString::to_string);
        report
            .timed_out
            .sort_by_key(std::string::ToString::to_string);
        report
    }

    /// Captures the parent's current settings for a persisted-history fork.
    /// Apply the returned settings to the new thread before starting its first turn.
    pub async fn fork_options_from_parent(
        &self,
        source_thread_id: ThreadId,
    ) -> CodexResult<(StartThreadOptions, CodexThreadSettingsOverrides)> {
        let parent = self.get_thread(source_thread_id).await?;
        let (config, settings, windows_sandbox_level, environments, reasoning_effort_pin) =
            parent.session.fork_config().await;
        let mut thread_extension_init = ExtensionDataInit::default();
        thread_extension_init.insert(reasoning_effort_pin);
        Ok((
            StartThreadOptions {
                history_mode: Some(settings.history_mode),
                environments: Some(settings.environments.into_requests().environment_requests),
                inherited_environments: Some(environments),
                client_mcp_extensions: parent.client_mcp_extensions(),
                disabled_plugin_ids: Some(settings.disabled_plugin_ids),
                thread_extension_init,
                turn_extension_init: settings.turn_extension_init,
                ..StartThreadOptions::new(config)
            },
            CodexThreadSettingsOverrides {
                collaboration_mode: Some(settings.collaboration_mode),
                windows_sandbox_level: Some(windows_sandbox_level),
                ..Default::default()
            },
        ))
    }

    /// Fork a legacy thread by snapshotting its full rollout history according to
    /// `snapshot` and starting a new thread with identical configuration
    /// (unless overridden by the caller's options). The new thread has a fresh id.
    /// Fork history replaces `options.initial_history`; hosts can supply the new
    /// task's instruction provider through the same options as thread creation.
    ///
    /// Paginated sources are rejected. Use [`ThreadStore::prepare_fork`] and
    /// [`Self::fork_prepared_thread`] for paginated thread forks.
    pub async fn fork_legacy_thread<S>(
        &self,
        snapshot: S,
        options: StartThreadOptions,
        path: PathBuf,
    ) -> CodexResult<NewThread>
    where
        S: Into<ForkSnapshot>,
    {
        let snapshot = snapshot.into();
        let history = self.legacy_initial_history_from_rollout_path(path).await?;
        self.fork_thread_from_history(snapshot, options, history)
            .await
    }

    async fn legacy_initial_history_from_rollout_path(
        &self,
        rollout_path: PathBuf,
    ) -> CodexResult<InitialHistory> {
        let requested_rollout_path = rollout_path.clone();
        let stored_thread = self
            .state
            .thread_store
            .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                rollout_path,
                include_archived: true,
                include_history: true,
            })
            .await
            .map_err(thread_store_rollout_read_error)?;
        stored_thread_to_initial_history(stored_thread, Some(requested_rollout_path))
    }

    /// Fork an existing thread from already-loaded store history.
    pub async fn fork_thread_from_history<S>(
        &self,
        snapshot: S,
        options: StartThreadOptions,
        history: InitialHistory,
    ) -> CodexResult<NewThread>
    where
        S: Into<ForkSnapshot>,
    {
        self.fork_thread_with_initial_history(
            options,
            ForkHistory {
                snapshot: snapshot.into(),
                initial_history: history,
                persistence: ForkPersistence::Copied,
            },
        )
        .await
    }

    /// Fork prepared reference-backed history using the same snapshot semantics as copied forks.
    pub async fn fork_prepared_thread(
        &self,
        options: StartThreadOptions,
        prepared: PreparedFork,
    ) -> CodexResult<NewThread> {
        let history = InitialHistory::Resumed(ResumedHistory {
            history_revision: None,
            conversation_id: prepared.source_thread_id,
            history: Arc::clone(&prepared.model_context),
            rollout_path: None,
        });
        let fork_persistence = ForkPersistence::Referenced {
            history_base: prepared.history_base,
            inherited_item_count: prepared.model_context.len(),
        };
        let result = self
            .fork_thread_with_initial_history(
                options,
                ForkHistory {
                    snapshot: ForkSnapshot::Interrupted,
                    initial_history: history,
                    persistence: fork_persistence,
                },
            )
            .await;
        drop(prepared);
        result
    }

    async fn fork_thread_with_initial_history(
        &self,
        mut options: StartThreadOptions,
        fork_history: ForkHistory,
    ) -> CodexResult<NewThread> {
        let ForkHistory {
            snapshot,
            initial_history: history,
            persistence: fork_persistence,
        } = fork_history;
        // `forked_from_id()` describes this history's existing lineage. When
        // forking a resumed thread, the child copies the resumed thread itself.
        let source_thread_id = match &history {
            InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
            InitialHistory::Forked(_) => history.forked_from_id(),
            InitialHistory::New | InitialHistory::Cleared => None,
        };
        // Capture the live source before later startup work can unload it. Keep
        // the fork's own providers, not the source's task-bound callback.
        let instructions = self
            .state
            .instructions_for_spawn(
                options
                    .session_source
                    .as_ref()
                    .unwrap_or(&self.state.session_source),
                /*parent_thread_id*/ None,
                source_thread_id,
                options.thread_instructions_provider.clone(),
            )
            .await;
        let multi_agent_version = self
            .state
            .effective_multi_agent_version_for_spawn(
                &history,
                /*session_source*/ None,
                /*parent_thread_id*/ None,
                source_thread_id,
                &options.config,
            )
            .await;
        let interrupted_marker = InterruptedTurnHistoryMarker::from_config_and_version(
            &options.config,
            multi_agent_version,
        );
        options.initial_history = fork_history_from_snapshot(snapshot, history, interrupted_marker);
        let agent_control = self.agent_control_for_config(&options.config);
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.state.auth_manager), agent_control);
        request.forked_from_thread_id = source_thread_id;
        request.fork_persistence = fork_persistence;
        request.inherited_instructions = Some(instructions);
        Box::pin(self.state.spawn_thread(request)).await
    }

    pub(crate) fn agent_control(&self) -> LocalAgentControl {
        LocalAgentControl::new(
            Arc::downgrade(&self.state),
            self.state.thread_id_generator.clone(),
            /*rollout_budget*/ None,
        )
    }

    fn agent_control_for_config(&self, config: &Config) -> LocalAgentControl {
        LocalAgentControl::new(
            Arc::downgrade(&self.state),
            self.state.thread_id_generator.clone(),
            config.rollout_budget.clone(),
        )
    }

    #[cfg(test)]
    pub(crate) fn captured_ops(&self) -> Vec<(ThreadId, Op)> {
        self.state
            .ops_log
            .as_ref()
            .and_then(|ops_log| {
                ops_log.lock().ok().map(|log| {
                    log.iter()
                        .filter_map(|(thread_id, op)| {
                            capture_test_op(op).map(|op| (*thread_id, op))
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}

impl ThreadManagerState {
    /// Real host root startup for controller fixtures, with the tested tree runtime.
    /// This does not create a delegated proof or accept a caller/source shortcut.
    #[cfg(test)]
    pub(crate) async fn start_root_with_control_for_tests(
        &self,
        options: StartThreadOptions,
        control: LocalAgentControl,
    ) -> CodexResult<NewThread> {
        if options
            .session_source
            .as_ref()
            .is_some_and(SessionSource::is_non_root_agent)
        {
            return Err(crate::agent::child_provider_authorization::invalid(
                "root fixture cannot use a delegated or internal source",
            ));
        }
        self.spawn_thread(ThreadSpawnRequest::new(
            options,
            Arc::clone(&self.auth_manager),
            control,
        ))
        .await
    }

    pub(crate) fn shared_thread_instructions_provider(
        &self,
        root_thread_id: ThreadId,
        provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> Option<Arc<dyn ThreadInstructionsProvider>> {
        self.shared_thread_instructions
            .for_root(root_thread_id, provider)
    }

    pub(crate) fn agent_graph_store(&self) -> Option<Arc<dyn AgentGraphStore>> {
        self.agent_graph_store.clone()
    }

    pub(crate) async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.threads
            .read()
            .await
            .iter()
            .filter_map(|(thread_id, thread)| {
                (!thread.session_source.is_internal()).then_some(*thread_id)
            })
            .collect()
    }

    /// List parent-child edges for currently loaded thread-spawn agents.
    pub(crate) async fn list_live_thread_spawn_edges(&self) -> Vec<(ThreadId, ThreadId)> {
        self.threads
            .read()
            .await
            .iter()
            .filter_map(|(thread_id, thread)| {
                if thread.session_source.is_internal() {
                    return None;
                }
                match &thread.session_source {
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id,
                        ..
                    }) => Some((*parent_thread_id, *thread_id)),
                    _ => None,
                }
            })
            .collect()
    }

    /// Fetch a thread by ID or return ThreadNotFound.
    pub(crate) async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        let threads = self.threads.read().await;
        match threads.get(&thread_id) {
            Some(thread) if !thread.session_source.is_internal() => Ok(thread.clone()),
            Some(_) | None => Err(CodexErr::ThreadNotFound(thread_id)),
        }
    }

    pub(crate) fn native_authority_storage(
        &self,
    ) -> Option<codex_thread_store::NativeAuthorityStorage> {
        self.thread_store.native_authority_storage()
    }

    pub(crate) async fn read_native_authority_epoch(
        &self,
        thread: ThreadId,
    ) -> CodexResult<Option<codex_thread_store::NativeAuthorityEpoch>> {
        self.thread_store
            .read_native_authority_epoch(thread)
            .await
            .map_err(|_| {
                crate::agent::child_provider_authorization::invalid(
                    "protected host authority ownership could not be read",
                )
            })
    }

    pub(crate) async fn read_native_authority_receipt(
        &self,
        thread: ThreadId,
    ) -> CodexResult<Option<codex_thread_store::NativeAuthorityReceipt>> {
        self.thread_store
            .read_native_authority_receipt(thread)
            .await
            .map_err(|_| {
                crate::agent::child_provider_authorization::invalid(
                    "protected host authority receipt could not be read",
                )
            })
    }

    async fn begin_session_authority_epoch(
        &self,
        session: &Session,
        epoch: codex_thread_store::NativeAuthorityEpoch,
    ) -> CodexResult<()> {
        let Some(writer) = session.live_thread() else {
            return Ok(());
        };
        match writer.begin_native_authority_epoch(epoch).await {
            Ok(()) | Err(ThreadStoreError::Unsupported { .. }) => Ok(()),
            Err(_) => Err(crate::agent::child_provider_authorization::invalid(
                "owned host authority epoch could not be fenced",
            )),
        }
    }

    pub(crate) async fn seal_native_authority_receipt(
        &self,
        receipt: codex_thread_store::NativeAuthorityReceipt,
    ) -> CodexResult<bool> {
        match self
            .thread_store
            .seal_native_authority_receipt(receipt)
            .await
        {
            Ok(sealed) => Ok(sealed),
            Err(ThreadStoreError::Unsupported { .. }) => Ok(false),
            Err(_) => Err(crate::agent::child_provider_authorization::invalid(
                "host authority ceiling could not be sealed",
            )),
        }
    }

    pub(crate) async fn read_stored_thread(
        &self,
        params: ReadThreadParams,
    ) -> CodexResult<StoredThread> {
        let thread_id = params.thread_id;
        self.thread_store
            .read_thread(params)
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                ThreadStoreError::InvalidRequest { message } => {
                    if message.starts_with("no rollout found for thread id ") {
                        CodexErr::ThreadNotFound(thread_id)
                    } else {
                        CodexErr::Fatal(format!(
                            "failed to read stored thread {thread_id}: invalid thread-store request: {message}"
                        ))
                    }
                }
                err => CodexErr::Fatal(format!("failed to read stored thread {thread_id}: {err}")),
            })
    }

    pub(crate) async fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> CodexResult<StoredModelContext> {
        let thread_id = params.thread_id;
        self.thread_store
            .load_latest_model_context(params)
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                err => CodexErr::Fatal(format!(
                    "failed to load model context for thread {thread_id}: {err}"
                )),
            })
    }

    /// Send an operation to a thread by ID.
    pub(crate) async fn send_op(
        &self,
        thread_id: ThreadId,
        op: Op,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
        message_delivery: Option<crate::agent_communication::MessageDelivery>,
    ) -> CodexResult<String> {
        let thread = self.get_thread(thread_id).await?;
        self.send_op_to_thread(&thread, op, parent_turn_id, root_turn_id, message_delivery)
            .await
    }

    pub(crate) async fn send_op_if_matches(
        &self,
        expected: &Arc<CodexThread>,
        op: Op,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
        message_delivery: Option<crate::agent_communication::MessageDelivery>,
    ) -> CodexResult<String> {
        let registered = self.get_thread(expected.session.thread_id).await?;
        if !Arc::ptr_eq(&registered, expected) {
            return Err(crate::agent::child_provider_authorization::invalid(
                "native recipient allocation was replaced",
            ));
        }
        self.send_op_to_thread(expected, op, parent_turn_id, root_turn_id, message_delivery)
            .await
    }

    async fn send_op_to_thread(
        &self,
        thread: &Arc<CodexThread>,
        op: Op,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
        message_delivery: Option<crate::agent_communication::MessageDelivery>,
    ) -> CodexResult<String> {
        self.send_op_to_thread_guarded(
            thread,
            op,
            parent_turn_id,
            root_turn_id,
            message_delivery,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_op_to_thread_guarded(
        &self,
        thread: &Arc<CodexThread>,
        op: Op,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
        message_delivery: Option<crate::agent_communication::MessageDelivery>,
        guard: Option<&crate::agent::child_provider_authorization::current::CurrentChildOperation>,
    ) -> CodexResult<String> {
        if let Some(guard) = guard {
            if !guard.matches_target(thread) {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "submission target allocation changed",
                ));
            }
        }
        let thread_id = thread.session.thread_id;
        let residency_guard = if matches!(op, Op::InterAgentCommunication { .. }) {
            thread
                .session
                .services
                .local_agent_runtime
                .pin_v2_residency(self, &thread)
                .await?
        } else {
            None
        };
        let captured_op = self.ops_log.as_ref().and_then(|_| capture_test_op(&op));
        let submitted = thread
            .io
            .submit_with_trace_guarded(
                op,
                /*trace*/ None,
                parent_turn_id,
                root_turn_id,
                residency_guard,
                message_delivery,
                guard,
            )
            .await;
        // This observer records admitted submissions, never an operation rejected
        // by the final policy fence after a queue-capacity wait.
        if submitted.is_ok()
            && let Some(ops_log) = &self.ops_log
            && let Ok(mut log) = ops_log.lock()
            && let Some(captured_op) = captured_op
        {
            log.push((thread_id, captured_op));
        }
        submitted
    }

    /// Remove a thread from the manager by ID, returning it when present.
    pub(crate) async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        let current = self.threads.read().await.get(thread_id).cloned()?;
        self.remove_thread_if_matches(thread_id, &current, || {})
            .await
    }

    pub(crate) async fn fence_native_subtree(
        &self,
        runtime: &LocalAgentRuntime,
        root: ThreadId,
        descendants: bool,
        unloaded: impl FnMut(ThreadId),
    ) -> Vec<(ThreadId, Arc<CodexThread>)> {
        self.capture_native_shutdown(runtime, root, descendants, unloaded, false)
            .await
            .threads
    }

    pub(crate) async fn fence_native_subtree_with_graph_stamp(
        &self,
        runtime: &LocalAgentRuntime,
        root: ThreadId,
        descendants: bool,
        unloaded: impl FnMut(ThreadId),
    ) -> NativeShutdownCapture {
        self.capture_native_shutdown(runtime, root, descendants, unloaded, true)
            .await
    }

    async fn capture_native_shutdown(
        &self,
        runtime: &LocalAgentRuntime,
        root: ThreadId,
        descendants: bool,
        mut unloaded: impl FnMut(ThreadId),
        explicit_graph_close: bool,
    ) -> NativeShutdownCapture {
        let threads = self.threads.write().await;
        let mut edges = runtime.child_provider_origins.ownership_edges();
        edges.extend(threads.iter().filter_map(|(id, thread)| {
            thread
                .session_source
                .parent_thread_id()
                .map(|parent| (*id, parent))
        }));
        let root_allocation = threads.get(&root).cloned();
        let (closed, graph_stamp) = if descendants {
            runtime.revoke_subtree_with_edges(root, edges, explicit_graph_close)
        } else {
            (
                std::collections::HashSet::from([root]),
                runtime.revoke_single_caller(root, explicit_graph_close),
            )
        };
        for id in closed.iter().filter(|id| !threads.contains_key(id)) {
            unloaded(*id);
        }
        let captured = threads
            .iter()
            .filter(|(id, _)| closed.contains(id))
            .map(|(id, thread)| (*id, Arc::clone(thread)))
            .collect();
        NativeShutdownCapture {
            threads: captured,
            root,
            root_allocation,
            graph_stamp,
        }
    }

    /// Manager map -> short canonical stamp check -> graph await with only map
    /// write held. New registration invalidates old close intent; same-registration
    /// teardown transfers that intent even after exact removal to absence.
    pub(crate) async fn persist_native_graph_closed(
        &self,
        runtime: &LocalAgentRuntime,
        capture: &NativeShutdownCapture,
    ) -> codex_agent_graph_store::AgentGraphStoreResult<()> {
        let threads = self.threads.write().await;
        // Absence after exact same-registration removal may inherit Closed;
        // it is NEVER sufficient without the canonical generation check below.
        let compatible_residency = match (&capture.root_allocation, threads.get(&capture.root)) {
            (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
            (None, None) => true,
            (Some(_), None) => capture.graph_stamp.can_transfer_from_loaded(),
            _ => false,
        };
        if !compatible_residency || !runtime.graph_close_is_current(&capture.graph_stamp) {
            return Ok(());
        }
        if let Some(store) = self.agent_graph_store() {
            store
                .set_thread_spawn_edge_status(
                    capture.root,
                    codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                )
                .await?;
        }
        drop(threads);
        Ok(())
    }

    pub(crate) async fn remove_thread_if_matches(
        &self,
        thread_id: &ThreadId,
        current: &Arc<CodexThread>,
        removed: impl FnOnce(),
    ) -> Option<Arc<CodexThread>> {
        let authority =
            crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &current.session,
                &current.session_source,
            )
            .await;
        if let Err(error) = crate::agent::child_provider_authorization::durable::seal_stopped(
            self, current, &authority,
        )
        .await
        {
            warn!(%error, "stopped native authority remains unavailable for cold reload");
        }
        let mut threads = self.threads.write().await;
        if !threads
            .get(thread_id)
            .is_some_and(|thread| Arc::ptr_eq(thread, current))
        {
            return None;
        }
        current.session.revoke_fresh_child_admission();
        current
            .session
            .services
            .local_agent_runtime
            .retire_authority(authority);
        removed();
        threads
            .remove(thread_id)
            .inspect(|thread| thread.session.services.mcp_runtime.revoke_ema())
    }

    pub(crate) async fn effective_multi_agent_version_for_spawn(
        &self,
        initial_history: &InitialHistory,
        session_source: Option<&SessionSource>,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        config: &Config,
    ) -> MultiAgentVersion {
        if let Some(multi_agent_version) = config.multi_agent_version_override() {
            return multi_agent_version;
        }
        self.initial_multi_agent_version_for_spawn(
            initial_history,
            session_source,
            parent_thread_id,
            forked_from_thread_id,
        )
        .await
        .unwrap_or_else(|| config.multi_agent_version_from_features())
    }

    async fn initial_multi_agent_version_for_spawn(
        &self,
        initial_history: &InitialHistory,
        session_source: Option<&SessionSource>,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
    ) -> Option<MultiAgentVersion> {
        let inherited_thread_id = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            })) => Some(*parent_thread_id),
            _ => match initial_history {
                InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
                InitialHistory::Forked(_) => forked_from_thread_id.or(parent_thread_id),
                InitialHistory::New | InitialHistory::Cleared => parent_thread_id,
            },
        };
        let inherited_multi_agent_version = match inherited_thread_id {
            Some(thread_id) => self
                .get_thread(thread_id)
                .await
                .ok()
                .and_then(|thread| thread.multi_agent_version()),
            None => None,
        };
        resolve_multi_agent_version(initial_history, inherited_multi_agent_version)
    }

    /// Selects instruction sources for a newly spawned runtime without loading providers.
    ///
    /// Fresh roots, cold resumes, and root forks retain the global and any supplied
    /// thread provider. The session's AgentsMdManager loads them at startup and
    /// subsequent context captures. Subagents inherit applied parent snapshots and
    /// any thread provider that explicitly opts into sharing with descendants.
    /// A root fork without a provider preserves the live source's thread snapshot,
    /// never its task-bound provider. Hosts must supply a provider for offline forks.
    /// Warm resumes retain the existing session and do not call this function.
    async fn instructions_for_spawn(
        &self,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        thread_provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> SessionInstructions {
        let inherited_thread_id = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) => Some(*parent_thread_id),
            _ => parent_thread_id.or(forked_from_thread_id),
        };
        let inherited = match inherited_thread_id {
            Some(thread_id) => match self.get_thread(thread_id).await {
                Ok(thread) => thread.session.inherited_instructions().await,
                Err(_) => SessionInstructions::default(),
            },
            None => SessionInstructions::default(),
        };
        if session_source.is_non_root_agent() {
            inherited
        } else {
            SessionInstructions {
                user_provider: Some(Arc::clone(&self.user_instructions_provider)),
                thread_provider,
                thread: inherited.thread,
                ..Default::default()
            }
        }
    }

    async fn inherited_originator_for_parent_thread(
        &self,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
    ) -> Option<String> {
        let inherited_thread_id = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) => Some(*parent_thread_id),
            _ => parent_thread_id.or(forked_from_thread_id),
        };
        let thread = self.get_thread(inherited_thread_id?).await.ok()?;
        let originator = thread.config_snapshot().await.originator;
        (!originator.is_empty()).then_some(originator)
    }

    async fn effective_originator(
        &self,
        initial_history: &InitialHistory,
        metrics_service_name: Option<&str>,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        parent_originator: Option<String>,
    ) -> String {
        let persisted_originator = initial_history.get_session_originator();
        let inherited_originator = match initial_history {
            InitialHistory::New | InitialHistory::Cleared => {
                self.inherited_originator_for_parent_thread(
                    session_source,
                    parent_thread_id,
                    forked_from_thread_id,
                )
                .await
            }
            InitialHistory::Forked(_) if persisted_originator.is_none() => {
                self.inherited_originator_for_parent_thread(
                    session_source,
                    parent_thread_id,
                    forked_from_thread_id,
                )
                .await
            }
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => None,
        };

        let env_originator = std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR)
            .is_ok()
            .then(|| originator().value);
        effective_originator_value(
            metrics_service_name,
            env_originator,
            persisted_originator,
            inherited_originator.or(parent_originator),
            originator().value,
        )
    }

    /// Start a newly spawned agent. Both history modes use the same options captured
    /// from the spawning turn; only history ownership and persistence differ.
    pub(crate) async fn spawn_child_thread(
        &self,
        mut options: StartThreadOptions,
        agent_control: LocalAgentControl,
        parent_thread_id: Option<ThreadId>,
        inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
        launch_permit: crate::agent::child_provider_authorization::ChildLaunchPermit,
    ) -> CodexResult<NewThread> {
        let (origin, session_id, native_admission) =
            launch_permit.consume(&agent_control.runtime, parent_thread_id, &options.config)?;
        let agent_control = agent_control.runtime.control(session_id);
        (options.client_mcp_extensions, options.prompt_cache_key) =
            self.inherited_child_settings(parent_thread_id).await;
        let forked = matches!(options.initial_history, InitialHistory::Forked(_));
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.auth_manager), agent_control);
        request.parent_thread_id = parent_thread_id;
        request.inherited_exec_policy = inherited_exec_policy;
        request.delegated_launch = true;
        request.native_admission = Some(native_admission);
        request.provider_origin = origin;
        if forked {
            request.forked_from_thread_id = parent_thread_id;
            request.fork_persistence = ForkPersistence::CopiedDeferred;
        }
        Box::pin(self.spawn_thread(request)).await
    }

    pub(crate) async fn resume_thread_with_history_with_source(
        &self,
        options: ResumeThreadWithHistoryOptions,
    ) -> CodexResult<NewThread> {
        let ResumeThreadWithHistoryOptions {
            authorization,
            config,
            prompt_cache_key,
            initial_history,
            agent_control,
            session_source,
            parent_thread_id,
            environment_selections,
            inherited_environments,
            inherited_instructions,
            inherited_exec_policy,
            client_mcp_extensions,
        } = options;
        // This private path is reached only after the controller has loaded and
        // checked host-owned rollout metadata. A supplied history/source at a
        // public root API never sets this delegated-start classification.
        let delegated_parent = match &session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: owner,
                ..
            }) => {
                if parent_thread_id != Some(*owner)
                    || initial_history
                        .get_resumed_parent_thread_id()
                        .is_some_and(|parent| parent != *owner)
                {
                    return Err(crate::agent::child_provider_authorization::invalid(
                        "resumed child ownership is inconsistent",
                    ));
                }
                Some(*owner)
            }
            _ => None,
        };
        let (provider_origin, native_admission) = if let Some(authorization) = authorization {
            let (origin, admission) = authorization
                .consume(
                    crate::agent::child_provider_authorization::ChildResumeAdmissionContext {
                        runtime: &agent_control.runtime,
                        config: &config,
                        history: &initial_history,
                        source: &session_source,
                        parent: parent_thread_id,
                        selections: &environment_selections,
                        inherited: inherited_environments.as_ref(),
                    },
                )
                .await?;
            (origin, Some(admission))
        } else {
            if delegated_parent.is_some()
                || initial_history.get_resumed_parent_thread_id().is_some()
            {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "saved child launch requires private host resume authorization",
                ));
            }
            (None, None)
        };
        let (parent_client_mcp_extensions, inherited_prompt_cache_key) =
            self.inherited_child_settings(parent_thread_id).await;
        let client_mcp_extensions = client_mcp_extensions.unwrap_or(parent_client_mcp_extensions);
        let prompt_cache_key = prompt_cache_key.unwrap_or(inherited_prompt_cache_key);
        let thread_source = initial_history.get_resumed_thread_source();
        let options = StartThreadOptions {
            prompt_cache_key,
            initial_history,
            session_source: Some(session_source),
            thread_source,
            environments: environment_selections.map(|selections| {
                selections
                    .into_iter()
                    .map(TurnEnvironmentSelection::into_request)
                    .collect()
            }),
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.auth_manager), agent_control);
        request.parent_thread_id = parent_thread_id;
        request.delegated_launch = delegated_parent.is_some();
        request.native_admission = native_admission;
        request.provider_origin = provider_origin;
        request.inherited_environments = inherited_environments;
        request.inherited_instructions = inherited_instructions;
        request.inherited_exec_policy = inherited_exec_policy;
        Box::pin(self.spawn_thread(request)).await
    }

    async fn inherited_child_settings(
        &self,
        parent_thread_id: Option<ThreadId>,
    ) -> (ClientMcpExtensions, PromptCacheKey) {
        let Some(parent_thread_id) = parent_thread_id else {
            return Default::default();
        };
        self.get_thread(parent_thread_id)
            .await
            .map(|parent| {
                (
                    parent.session.services.client_mcp_extensions.clone(),
                    parent
                        .session
                        .services
                        .model_client
                        .prompt_cache_key_policy(),
                )
            })
            .unwrap_or_default()
    }

    /// Spawn a new thread with optional history and register it with the manager.
    async fn spawn_thread(&self, mut request: ThreadSpawnRequest) -> CodexResult<NewThread> {
        let explicit_native = matches!(
            request
                .options
                .session_source
                .as_ref()
                .unwrap_or(&self.session_source),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        );
        let resumed_native = request
            .options
            .initial_history
            .get_resumed_parent_thread_id()
            .is_some()
            || request
                .options
                .initial_history
                .get_resumed_session_sources()
                .is_some_and(|(source, _)| {
                    matches!(
                        source,
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                    )
                });
        // A source label never grants a private launch classification. Genuine
        // internal helpers carry a captured parent and only New/Forked history;
        // owned resumed history must pass the validated child-resume path.
        if (explicit_native || resumed_native) && !request.delegated_launch {
            return Err(crate::agent::child_provider_authorization::invalid(
                "delegated thread source requires the private native launch path",
            ));
        }
        if request
            .options
            .config
            .prepared_child_authorization
            .is_some()
        {
            return Err(crate::agent::child_provider_authorization::invalid(
                "prepared delegated configuration cannot be used as an independent root or host session",
            ));
        }
        if request.delegated_launch && request.native_admission.is_none() {
            return Err(crate::agent::child_provider_authorization::invalid(
                "native startup requires its private final admission authority",
            ));
        }
        if let Some(admission) = &request.native_admission {
            admission.check_current()?;
        }
        let cancellation = request
            .options
            .config
            .config_layer_stack
            .mcp_ema_cancellation()
            .for_startup();
        request.options.config.config_layer_stack = request
            .options
            .config
            .config_layer_stack
            .with_mcp_ema_cancellation(cancellation);
        let runtime = request.agent_control.runtime().clone();
        let membership = runtime.admit_start()?;
        let owns_startup = request.startup.is_none();
        let startup_state = Arc::clone(
            request
                .startup
                .get_or_insert_with(|| Arc::new(SessionStartup::default())),
        );
        if let Some(admission) = request.native_admission.take() {
            startup_state.set_native_admission(admission)?;
        }
        startup_state.hold_membership(membership);
        let startup_guard =
            owns_startup.then(|| SessionStartupGuard::new(Arc::clone(&startup_state)));
        let ThreadSpawnRequest {
            delegated_launch,
            native_admission: _,
            provider_origin,
            startup,
            options,
            auth_manager,
            agent_control,
            parent_thread_id,
            parent_originator,
            forked_from_thread_id,
            fork_persistence,
            inherited_environments,
            inherited_instructions,
            inherited_exec_policy,
            user_shell_override,
        } = request;
        let history_initialization = if matches!(
            &options.initial_history,
            InitialHistory::New | InitialHistory::Forked(_)
        ) && let Some(source_thread_id) = forked_from_thread_id
        {
            if let Ok(source) = self.get_thread(source_thread_id).await
                && source.agent_status().await != AgentStatus::Shutdown
            {
                HistoryInitialization::WarmFork
            } else {
                HistoryInitialization::ColdFork
            }
        } else {
            HistoryInitialization::from_history(&options.initial_history)
        };
        let StartThreadOptions {
            mut config,
            prompt_cache_key,
            thread_instructions_provider,
            allow_provider_model_fallback,
            initial_history,
            resume_preparation,
            internal_parent: _,
            history_mode,
            session_source,
            thread_source,
            dynamic_tools,
            metrics_service_name,
            parent_trace,
            environments: environment_requests,
            inherited_environments: captured_environments,
            user_instructions: supplied_user_instructions,
            mut thread_extension_init,
            turn_extension_init,
            client_mcp_extensions,
            mut reserved_thread_id,
            disabled_plugin_ids,
        } = options;
        if resume_preparation.is_some()
            && (config.ephemeral || !matches!(&initial_history, InitialHistory::Resumed(_)))
        {
            return Err(CodexErr::InvalidRequest(
                "thread store resume preparation requires a persisted resumed thread".to_string(),
            ));
        }
        let inherited_environments = captured_environments.or(inherited_environments);
        let session_source = session_source.unwrap_or_else(|| self.session_source.clone());
        // Older callers and saved reviewers identify isolation through their source.
        // New internal callers supply an explicit runtime policy before startup.
        let isolation = thread_extension_init
            .get::<codex_extension_api::SessionIsolation>()
            .map(|policy| *policy)
            .unwrap_or_else(|| {
                if crate::guardian::is_basic_session_source(&session_source) {
                    codex_extension_api::SessionIsolation::Isolated
                } else {
                    codex_extension_api::SessionIsolation::Inherit
                }
            });
        thread_extension_init.insert(isolation);
        let environment_requests = environment_requests
            .or_else(|| {
                let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) = &session_source
                else {
                    return None;
                };
                inherited_environments.as_ref().map(|snapshot| {
                    snapshot
                        .inheritable_selections()
                        .into_iter()
                        .map(TurnEnvironmentSelection::into_request)
                        .collect()
                })
            })
            .unwrap_or_else(|| {
                default_thread_environment_requests(
                    self.environment_manager.as_ref(),
                    &config.cwd,
                    &config.workspace_roots,
                )
            });
        let is_resumed_thread = matches!(&initial_history, InitialHistory::Resumed(_));
        if reserved_thread_id.is_some() && matches!(&initial_history, InitialHistory::Resumed(_)) {
            return Err(CodexErr::InvalidRequest(
                "reserved thread ID cannot be used when resuming a thread".to_string(),
            ));
        }
        if let InitialHistory::Resumed(resumed) = &initial_history {
            let stopped = {
                self.threads
                    .read()
                    .await
                    .get(&resumed.conversation_id)
                    .cloned()
            }
            .filter(|thread| !thread.is_running());
            let stopped_authority = if let Some(thread) = &stopped {
                Some(
                    crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                        &thread.session,
                        &thread.session_source,
                    )
                    .await,
                )
            } else {
                None
            };
            if let (Some(thread), Some(authority)) = (&stopped, &stopped_authority)
                && let Err(error) =
                    crate::agent::child_provider_authorization::durable::seal_stopped(
                        self, thread, authority,
                    )
                    .await
            {
                warn!(%error, "replaced native authority remains unavailable for cold reload");
            }
            let mut threads = self.threads.write().await;
            if let Some(thread) = threads.get(&resumed.conversation_id).cloned() {
                if thread.is_running() {
                    // The parent's pool still owns this runtime and its rollout writer.
                    // Returning it would report success without allowing client access.
                    if matches!(
                        thread.session_source,
                        SessionSource::Internal(InternalSessionSource::Guardian)
                    ) {
                        return Err(CodexErr::InvalidRequest(
                            "cannot resume a live Guardian reviewer; use thread/read to inspect it, or resume after its parent is unloaded".to_owned(),
                        ));
                    }
                    if let Some(requested_rollout_path) = resumed.rollout_path.as_deref()
                        && thread.rollout_path().as_deref() != Some(requested_rollout_path)
                    {
                        return Err(CodexErr::InvalidRequest(format!(
                            "thread {} is already running with a different rollout path",
                            resumed.conversation_id
                        )));
                    }
                    let session_configured =
                        thread.startup_metadata().to_session_configured_event();
                    startup_state.release_membership();
                    if let Some(startup_guard) = startup_guard {
                        startup_guard.disarm();
                    }
                    return Ok(NewThread {
                        thread_id: resumed.conversation_id,
                        session_configured,
                        thread,
                    });
                }
                if !stopped
                    .as_ref()
                    .is_some_and(|captured| Arc::ptr_eq(captured, &thread))
                {
                    return Err(crate::agent::child_provider_authorization::invalid(
                        "stopped runtime changed while capturing its authority; retry resume",
                    ));
                }
                if let Some(authority) = stopped_authority {
                    thread
                        .session
                        .services
                        .local_agent_runtime
                        .retire_authority(authority);
                }
                thread.session.revoke_fresh_child_admission();
                threads.remove(&resumed.conversation_id);
                thread.session.services.mcp_runtime.revoke_ema();
            }
        }
        if let InitialHistory::Resumed(saved) = &initial_history {
            // A denied final admission must leave the stopped runtime's sealed
            // receipt intact. Session fences this exact target after admission
            // and before startup effects; inline Session paths remain unmarked.
            startup_state.defer_resume_fence(saved.conversation_id)?;
        }
        // A host factory is injectable code, so guard its invocation as well
        // as the final Session startup after the remaining metadata awaits.
        startup_state.check_native_current()?;
        let agent_control = match (&self.agent_control_factory, agent_control) {
            (Some(factory), AgentControlInit::Local(local)) => {
                let thread_id = match &initial_history {
                    InitialHistory::Resumed(resumed) => resumed.conversation_id,
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        *reserved_thread_id
                            .get_or_insert_with(|| local.runtime.generate_thread_id())
                    }
                };
                AgentControlInit::Provided {
                    control: factory(thread_id).await?,
                    runtime: local.runtime,
                }
            }
            (_, control) => control,
        };
        let parent_thread_id = parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id())
            .or(match &session_source {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                }) => Some(*parent_thread_id),
                _ => None,
            });
        // Host controllers can already be shared with live threads. Publish root settings
        // only after registration succeeds, so a failed start cannot update their tree.
        let initial_host_config = (self.agent_control_factory.is_some()
            && parent_thread_id.is_none())
        .then(|| AgentConfigUpdate::ServiceTier(config.service_tier.clone()));
        // Both resume entry points must restore identities before children can be loaded lazily.
        if let InitialHistory::Resumed(resumed) = &initial_history
            && initial_history.get_multi_agent_version() == Some(MultiAgentVersion::V2)
            && !session_source.is_non_root_agent()
            && let AgentControlInit::Local(control) = &agent_control
        {
            control
                .restore_v2_agent_metadata(&config, resumed.conversation_id)
                .await;
        }
        let (instructions, inherited_exec_policy, extensions, mcp_manager, multi_agent_version) =
            if isolation == codex_extension_api::SessionIsolation::Isolated {
                let extensions = thread_extension_init
                    .get::<codex_extension_api::IsolatedSessionExtensions<Config>>()
                    .map(|extensions| Arc::clone(&extensions.0))
                    .unwrap_or_else(empty_extension_registry);
                let mcp_manager = McpManager::new_with_extensions(
                    Arc::clone(&self.plugins_manager),
                    Arc::clone(&extensions),
                    CodexAppsToolsCache::default(),
                );
                (
                    inherited_instructions.unwrap_or_default(),
                    None,
                    extensions,
                    Arc::new(mcp_manager),
                    Some(MultiAgentVersion::Disabled),
                )
            } else {
                (
                    match inherited_instructions {
                        Some(instructions) => instructions,
                        None => {
                            self.instructions_for_spawn(
                                &session_source,
                                parent_thread_id,
                                forked_from_thread_id,
                                thread_instructions_provider,
                            )
                            .await
                        }
                    },
                    inherited_exec_policy,
                    Arc::clone(&self.extensions),
                    Arc::clone(&self.mcp_manager),
                    self.initial_multi_agent_version_for_spawn(
                        &initial_history,
                        Some(&session_source),
                        parent_thread_id,
                        forked_from_thread_id,
                    )
                    .await,
                )
            };
        let mut instructions = instructions;
        if let Some(supplied) = supplied_user_instructions {
            instructions.user = supplied.instructions;
            instructions.user_provider = None;
            config.startup_warnings.extend(supplied.warnings);
        }
        let parent_rollout_thread_trace = self
            .parent_rollout_thread_trace_for_source(&session_source, &initial_history)
            .await;
        let tracked_session_source = session_source.clone();
        let originator = self
            .effective_originator(
                &initial_history,
                metrics_service_name.as_deref(),
                &session_source,
                parent_thread_id,
                forked_from_thread_id,
                parent_originator,
            )
            .await;
        let source_changed_during_startup = Arc::new(AtomicBool::new(false));
        {
            let mut starting = self
                .starting_mcp_runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            starting.retain(|runtime| runtime.strong_count() != 0);
            starting.push(Arc::downgrade(&source_changed_during_startup));
        }
        let windows_sandbox_proxy_settings_mode = if matches!(
            &session_source,
            SessionSource::Internal(InternalSessionSource::Guardian)
        ) {
            codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve
        } else {
            codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile
        };
        let attachment_source =
            forked_from_thread_id.filter(|_| matches!(&initial_history, InitialHistory::Forked(_)));
        let models_manager =
            crate::agent::child_config::take_prepared_child_models(&mut config, &auth_manager)
                .map_err(CodexErr::InvalidRequest)?
                .unwrap_or_else(|| Arc::clone(&self.models_manager));
        let spawn_result = Session::spawn(SessionSpawnArgs {
            startup,
            config,
            prompt_cache_key,
            allow_provider_model_fallback,
            instructions,
            installation_id: self.installation_id.clone(),
            auth_manager,
            models_manager,
            git_root_discovery: Arc::clone(&self.git_root_discovery),
            environment_manager: Arc::clone(&self.environment_manager),
            skills_service: Arc::clone(&self.skills_service),
            plugins_manager: Arc::clone(&self.plugins_manager),
            mcp_manager,
            code_mode_session_provider: Arc::clone(&self.code_mode_session_provider),
            extensions,
            conversation_history: initial_history,
            resume_preparation,
            history_initialization,
            disabled_plugin_ids,
            requested_history_mode: history_mode,
            fork_persistence,
            // Keep only the manager registration internal. The session and its saved
            // history retain Guardian's existing identity for metadata, filtering and resume.
            session_source: match session_source {
                SessionSource::Internal(InternalSessionSource::Guardian) => {
                    SessionSource::SubAgent(SubAgentSource::Other(
                        crate::guardian::GUARDIAN_REVIEWER_NAME.to_owned(),
                    ))
                }
                source => source,
            },
            forked_from_thread_id,
            parent_thread_id,
            thread_source: thread_source.clone(),
            originator,
            agent_control,
            dynamic_tools,
            metrics_service_name,
            inherited_environments,
            inherited_exec_policy,
            parent_rollout_thread_trace,
            user_shell_override,
            parent_trace,
            environment_requests,
            thread_extension_init,
            turn_extension_init,
            client_mcp_extensions,
            reserved_thread_id,
            analytics_events_client: self.analytics_events_client.clone(),
            image_store: Arc::clone(&self.image_store),
            thread_store: Arc::clone(&self.thread_store),
            attestation_provider: self.attestation_provider.clone(),
            external_time_provider: self.external_time_provider.clone(),
            inherited_multi_agent_version: multi_agent_version,
            git_enrichment_policy: if matches!(
                &tracked_session_source,
                SessionSource::Internal(InternalSessionSource::Guardian)
            ) {
                GitEnrichmentPolicy::Skip
            } else {
                GitEnrichmentPolicy::Fresh
            },
            windows_sandbox_proxy_settings_mode,
        })
        .await;
        let (session, io) = match spawn_result {
            Ok(spawned) => spawned,
            Err(error) => {
                if let Some(startup_guard) = startup_guard {
                    startup_guard.cleanup().await;
                }
                return Err(error);
            }
        };
        if let Some(source_thread_id) = attachment_source
            && session.live_thread().is_some()
            && self.thread_store.supports_thread_attachments()
        {
            // Fork initialization already handles persistence. Copy current membership before
            // registration, but do not fail the conversation fork for attachment metadata errors.
            let thread_id = session.thread_id();
            if let Err(error) = self
                .thread_store
                .copy_thread_attachments(source_thread_id, thread_id)
                .await
            {
                warn!(%thread_id, %source_thread_id, %error, "failed to copy fork attachments");
            }
        }
        // Enable Full Access form input only after session startup so a required MCP server cannot
        // block startup while waiting for form input.
        if session
            .services
            .client_mcp_extensions
            .contains(OPENAI_STANDARD_FORM_INPUT_EXTENSION_ID)
            && matches!(thread_source.as_ref(), Some(ThreadSource::User))
            && !tracked_session_source.is_non_root_agent()
        {
            session.services.mcp_runtime.enable_full_access_form_input();
        }
        let finalize_result = self
            .finalize_thread_spawn(
                session,
                io,
                tracked_session_source,
                initial_host_config,
                delegated_launch.then_some(parent_thread_id).flatten(),
                provider_origin,
                &startup_state,
            )
            .await;
        let new_thread = match finalize_result {
            Ok(new_thread) => new_thread,
            Err(error) => {
                if let Some(startup_guard) = startup_guard {
                    startup_guard.cleanup().await;
                }
                return Err(error);
            }
        };
        startup_state.release_membership();
        if let Some(startup_guard) = startup_guard {
            startup_guard.disarm();
        }
        new_thread.thread.emit_thread_ready_lifecycle().await;
        if source_changed_during_startup.load(Ordering::Acquire) {
            new_thread.thread.session.request_mcp_runtime_refresh();
        }
        if is_resumed_thread {
            new_thread.thread.emit_thread_resume_lifecycle().await;
        }
        Ok(new_thread)
    }

    async fn finalize_thread_spawn(
        &self,
        session: Arc<Session>,
        io: SessionIo,
        session_source: SessionSource,
        initial_host_config: Option<AgentConfigUpdate>,
        delegated_parent: Option<ThreadId>,
        provider_origin: Option<codex_config::child_provider::VerifiedChildProviderOrigin>,
        startup: &SessionStartup,
    ) -> CodexResult<NewThread> {
        let thread_id = session.thread_id();
        let event = io.next_event().await?;
        let session_configured = match event {
            Event {
                id,
                msg: EventMsg::SessionConfigured(session_configured),
            } if id == INITIAL_SUBMIT_ID => session_configured,
            _ => {
                return Err(CodexErr::SessionConfiguredNotFirstEvent);
            }
        };

        let retained_authority =
            crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
                &session,
                &session_source,
            )
            .await;
        startup.check_native_current()?;
        if (!session_source.is_non_root_agent()
            || matches!(
                session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ))
            && let Err(error) = self
                .begin_session_authority_epoch(&session, retained_authority.epoch())
                .await
        {
            if let Err(shutdown_error) = io.shutdown_and_wait().await {
                warn!(%shutdown_error, "failed to stop runtime after authority-store fencing failure");
            }
            return Err(error);
        }
        let ema_cancellation = session
            .get_config()
            .await
            .config_layer_stack
            .mcp_ema_cancellation()
            .clone();
        let runtime = session.services.local_agent_runtime.clone();
        let mut persistence = startup.persistence.lock().await;
        {
            let mut threads = self.threads.write().await;
            if let std::collections::hash_map::Entry::Vacant(e) = threads.entry(thread_id) {
                return startup.commit_registration(&runtime, retained_authority, || {
                    let origins = &session.services.local_agent_runtime.child_provider_origins;
                    if let Some(parent) = delegated_parent {
                        origins.publish_child(thread_id, parent, provider_origin);
                    } else if !session_source.is_non_root_agent() {
                        origins.publish_root(thread_id, &session_configured.model_provider_id);
                    }
                    if let Some(update) = initial_host_config {
                        session
                            .services
                            .agent_control
                            .propagate_config_update(update);
                    }
                    ema_cancellation.publish();
                    let thread = Arc::new(CodexThread::new(
                        session,
                        io,
                        ThreadStartupMetadata::from(&session_configured),
                        session_configured.rollout_path.clone(),
                        session_source,
                    ));
                    e.insert(thread.clone());
                    persistence.commit();
                    Ok(NewThread {
                        thread_id,
                        thread,
                        session_configured,
                    })
                });
            }
        }

        if let Err(err) = io.shutdown_and_wait().await {
            warn!("failed to shut down duplicate thread {thread_id}: {err}");
        }
        Err(CodexErr::InvalidRequest(format!(
            "thread {thread_id} is already running"
        )))
    }

    pub(crate) fn notify_thread_created(&self, thread_id: ThreadId) {
        let _ = self.thread_created_tx.send(thread_id);
    }

    async fn parent_rollout_thread_trace_for_source(
        &self,
        session_source: &SessionSource,
        initial_history: &InitialHistory,
    ) -> codex_rollout_trace::ThreadTraceContext {
        // A fresh v2 child belongs to the same rollout tree as its parent, so
        // session startup derives its child trace from the parent's thread
        // context. Resumed children already have a prior `ThreadStarted` event
        // for this thread id; deriving a child trace during resume would write
        // that start event again and make the bundle unreplayable.
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = session_source
        else {
            return codex_rollout_trace::ThreadTraceContext::disabled();
        };
        if matches!(initial_history, InitialHistory::Resumed(_)) {
            return codex_rollout_trace::ThreadTraceContext::disabled();
        }
        // Parent lookup can fail if the parent was closed or released between
        // spawn preparation and session construction. Tracing is diagnostic, so
        // that race should not block child creation; the child simply starts
        // without a parent rollout trace.
        self.get_thread(*parent_thread_id)
            .await
            .ok()
            .map(|thread| thread.session.services.rollout_thread_trace.clone())
            .unwrap_or_else(codex_rollout_trace::ThreadTraceContext::disabled)
    }
}

fn stored_thread_to_initial_history(
    stored_thread: StoredThread,
    rollout_path: Option<PathBuf>,
) -> CodexResult<InitialHistory> {
    let thread_id = stored_thread.thread_id;
    let history = stored_thread.history.ok_or_else(|| {
        CodexErr::Fatal(format!(
            "thread {thread_id} did not include persisted history"
        ))
    })?;
    Ok(InitialHistory::Resumed(ResumedHistory {
        history_revision: history.revision,
        conversation_id: thread_id,
        history: Arc::new(history.items),
        rollout_path: rollout_path.or(stored_thread.rollout_path),
    }))
}

fn thread_store_rollout_read_error(err: ThreadStoreError) -> CodexErr {
    match err {
        ThreadStoreError::ThreadNotFound { thread_id } => CodexErr::ThreadNotFound(thread_id),
        ThreadStoreError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        err => CodexErr::Fatal(format!("failed to read thread by rollout path: {err}")),
    }
}

fn thread_store_metadata_update_error(thread_id: ThreadId, err: ThreadStoreError) -> CodexErr {
    match err {
        ThreadStoreError::ThreadNotFound { thread_id } => CodexErr::ThreadNotFound(thread_id),
        ThreadStoreError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        ThreadStoreError::Unsupported { operation } => CodexErr::UnsupportedOperation(format!(
            "thread metadata update is not supported by this store: {operation}"
        )),
        err => CodexErr::Fatal(format!(
            "failed to update thread metadata {thread_id}: {err}"
        )),
    }
}

/// Return a fork snapshot cut strictly before the nth user message (0-based).
///
/// Out-of-range values keep the full committed history at a turn boundary, but
/// when the source thread is currently mid-turn they fall back to cutting
/// before the active turn's opening boundary so the fork omits the unfinished
/// suffix entirely.
fn truncate_before_nth_user_message(
    history: InitialHistory,
    n: usize,
    snapshot_state: &SnapshotTurnState,
) -> InitialHistory {
    let mut items = match history {
        InitialHistory::New | InitialHistory::Cleared => Vec::new(),
        InitialHistory::Resumed(resumed) => Arc::unwrap_or_clone(resumed.history),
        InitialHistory::Forked(items) => items,
    };
    let user_positions = truncation::user_message_positions_in_rollout(&items);
    let rolled = if snapshot_state.ends_mid_turn && n >= user_positions.len() {
        if let Some(cut_idx) = snapshot_state
            .active_turn_start_index
            .or_else(|| user_positions.last().copied())
        {
            items.truncate(cut_idx);
            items
        } else {
            items
        }
    } else {
        truncation::truncate_rollout_before_nth_user_message_from_start(items, n)
    };

    if rolled.is_empty() {
        InitialHistory::New
    } else {
        InitialHistory::Forked(rolled)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotTurnState {
    ends_mid_turn: bool,
    active_turn_id: Option<String>,
    active_turn_started_at: Option<i64>,
    active_turn_root_id: Option<String>,
    active_turn_start_index: Option<usize>,
}

fn snapshot_turn_state(history: &InitialHistory) -> SnapshotTurnState {
    let rollout_items = history.get_rollout_items();
    let mut builder = ThreadHistoryBuilder::new();
    for item in rollout_items {
        builder.handle_rollout_item(item);
    }
    let active_turn_id = builder.active_turn_id_if_explicit();
    if builder.has_active_turn() && active_turn_id.is_some() {
        let active_turn_snapshot = builder.active_turn_snapshot();
        if active_turn_snapshot
            .as_ref()
            .is_some_and(|turn| turn.status != TurnStatus::InProgress)
        {
            return SnapshotTurnState {
                ends_mid_turn: false,
                active_turn_id: None,
                active_turn_started_at: None,
                active_turn_root_id: None,
                active_turn_start_index: None,
            };
        }

        return SnapshotTurnState {
            ends_mid_turn: true,
            active_turn_id,
            active_turn_started_at: active_turn_snapshot
                .as_ref()
                .and_then(|turn| turn.started_at),
            active_turn_root_id: active_turn_snapshot.and_then(|turn| turn.root_turn_id),
            active_turn_start_index: builder.active_turn_start_index(),
        };
    }

    let Some(last_user_position) = truncation::user_message_positions_in_rollout(rollout_items)
        .last()
        .copied()
    else {
        return SnapshotTurnState {
            ends_mid_turn: false,
            active_turn_id: None,
            active_turn_started_at: None,
            active_turn_root_id: None,
            active_turn_start_index: None,
        };
    };

    // Synthetic fork/resume histories can contain user/assistant response items
    // without explicit turn lifecycle events. If the persisted snapshot has no
    // terminating boundary after its last user message, treat it as mid-turn.
    SnapshotTurnState {
        ends_mid_turn: !rollout_items[last_user_position + 1..].iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
            )
        }),
        active_turn_id: None,
        active_turn_started_at: None,
        active_turn_root_id: None,
        active_turn_start_index: None,
    }
}

pub(crate) fn fork_history_from_snapshot(
    snapshot: ForkSnapshot,
    history: InitialHistory,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let snapshot_state = snapshot_turn_state(&history);
    match snapshot {
        ForkSnapshot::TruncateBeforeNthUserMessage(nth_user_message) => {
            truncate_before_nth_user_message(history, nth_user_message, &snapshot_state)
        }
        ForkSnapshot::Interrupted => {
            let history = match history {
                InitialHistory::New => InitialHistory::New,
                InitialHistory::Cleared => InitialHistory::Cleared,
                InitialHistory::Forked(history) => InitialHistory::Forked(history),
                InitialHistory::Resumed(resumed) => {
                    InitialHistory::Forked(Arc::unwrap_or_clone(resumed.history))
                }
            };
            if snapshot_state.ends_mid_turn {
                append_interrupted_boundary(
                    history,
                    snapshot_state.active_turn_id,
                    snapshot_state.active_turn_root_id,
                    snapshot_state.active_turn_started_at,
                    interrupted_marker,
                )
            } else {
                history
            }
        }
    }
}

/// Append the same persisted interrupt boundary used by the live interrupt path
/// to an existing fork snapshot after the source thread has been confirmed to
/// be mid-turn.
fn append_interrupted_boundary(
    history: InitialHistory,
    turn_id: Option<String>,
    root_turn_id: Option<String>,
    started_at: Option<i64>,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let aborted_event = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        root_turn_id,
        turn_id,
        reason: TurnAbortReason::Interrupted,
        error: None,
        started_at,
        completed_at: None,
        duration_ms: None,
    }));

    match history {
        InitialHistory::New | InitialHistory::Cleared => {
            let mut history = Vec::new();
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Forked(mut history) => {
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Resumed(resumed) => {
            let mut history = Arc::unwrap_or_clone(resumed.history);
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
    }
}

#[cfg(test)]
#[path = "thread_manager_tests.rs"]
mod tests;
